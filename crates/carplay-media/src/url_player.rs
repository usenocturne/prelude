use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video::{VideoFrameExt, VideoFrameRef, VideoInfo};

use super::{PCM_INPUT_CHANNELS, PCM_INPUT_SAMPLE_RATE, PcmInput, gst_error, make, set_checked};
use crate::{Error, LinuxDisplayHandle, Result};

const BUS_POLL: gst::ClockTime = gst::ClockTime::from_mseconds(100);
const STATUS_INTERVAL: Duration = Duration::from_secs(5);
const VIDEO_QUEUE_NS: u64 = 2_000_000_000;
const SCALER_THREADS: u32 = 2;
const CLOCK_ALIGNMENT_THRESHOLD: gst::ClockTime = gst::ClockTime::from_seconds(1);
const CLOCK_ALIGNMENT_MARGIN: gst::ClockTime = gst::ClockTime::from_mseconds(100);

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UrlPlayerStatus {
    pub ready: bool,
    pub playing: bool,
    pub ended: bool,
    pub failed: bool,
    pub position_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub buffered_percent: Option<u8>,
}

#[derive(Debug, Default)]
struct Counters {
    frames: AtomicU64,
    undrawable: AtomicU64,
    audio_buffers: AtomicU64,
    decoded: AtomicU64,
    first_running_ms: AtomicU64,
    last_running_ms: AtomicU64,
}

#[derive(Debug, Default)]
struct Shared {
    wanted_playing: bool,
    filling: bool,
    ready: bool,
    ended: bool,
    failed: bool,
    buffered_percent: Option<u8>,
    pending_seek: Option<f64>,
}

impl Shared {
    fn target_state(&self) -> gst::State {
        if self.wanted_playing && !self.filling {
            gst::State::Playing
        } else {
            gst::State::Paused
        }
    }
}

pub struct UrlVideoPlayer {
    pipeline: gst::Pipeline,
    volume: gst::Element,
    shared: Arc<Mutex<Shared>>,
    stopped: Arc<AtomicBool>,
    bus_worker: Option<JoinHandle<()>>,
    display: LinuxDisplayHandle,
    display_source: u64,
}

impl std::fmt::Debug for UrlVideoPlayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UrlVideoPlayer")
            .field("display_source", &self.display_source)
            .finish_non_exhaustive()
    }
}

impl UrlVideoPlayer {
    pub fn start(
        url: &str,
        start_seconds: f64,
        muted: bool,
        display: LinuxDisplayHandle,
        audio: Option<PcmInput>,
    ) -> Result<Self> {
        gst::init()
            .map_err(|error| Error::Backend(format!("cannot initialize GStreamer: {error}")))?;
        let pipeline = gst::Pipeline::with_name("carplay-url-video");
        let streams_aware = gst::ElementFactory::find("hlsdemux2")
            .is_some_and(|factory| factory.rank() > gst::Rank::NONE);
        let decoder = if streams_aware {
            "uridecodebin3"
        } else {
            "uridecodebin"
        };
        let source = make(decoder, "url-video-source")?;
        set_checked(&source, "uri", url)?;
        let video_offset = audio
            .as_ref()
            .map_or(Duration::ZERO, PcmInput::output_latency);
        let display_source = display.register_video_source()?;
        let counters = Arc::new(Counters::default());
        let built = (|| {
            let (video_input, video_sink) = video_branch(&pipeline, video_offset)?;
            let (audio_input, volume, audio_sink) = audio_branch(&pipeline, audio.is_some())?;
            set_checked(&volume, "mute", muted)?;
            pipeline
                .add(&source)
                .map_err(gst_error("cannot add the URL video source"))?;
            count_decoded_video(&video_input, Arc::clone(&counters));
            align_to_clock(&pipeline, [&video_input, &audio_input]);
            link_decoded_pads(&pipeline, &source, video_input, audio_input);
            pin_hardware_decoder_io(&pipeline);
            install_video_renderer(
                &video_sink,
                display.clone(),
                display_source,
                Arc::clone(&counters),
            );
            if let (Some(sink), Some(audio)) = (audio_sink, audio) {
                install_audio_forwarder(&sink, audio, Arc::clone(&counters));
            }
            Ok(volume)
        })();
        let volume = match built {
            Ok(volume) => volume,
            Err(error) => {
                let _ = pipeline.set_state(gst::State::Null);
                let _ = display.unregister_video_source(display_source);
                return Err(error);
            }
        };
        let shared = Arc::new(Mutex::new(Shared {
            pending_seek: (start_seconds > 0.0).then_some(start_seconds),
            ..Shared::default()
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let bus = pipeline
            .bus()
            .ok_or_else(|| Error::Backend("the URL video pipeline has no bus".to_owned()))?;
        let worker_pipeline = pipeline.clone();
        let worker_shared = Arc::clone(&shared);
        let worker_stopped = Arc::clone(&stopped);
        let bus_worker = thread::Builder::new()
            .name("prelude-url-video".to_owned())
            .spawn(move || {
                watch_bus(
                    &bus,
                    &worker_pipeline,
                    &worker_shared,
                    &worker_stopped,
                    &counters,
                );
            })
            .map_err(|error| Error::Backend(format!("cannot watch the URL video bus: {error}")))?;
        let player = Self {
            pipeline,
            volume,
            shared,
            stopped,
            bus_worker: Some(bus_worker),
            display,
            display_source,
        };
        player
            .pipeline
            .set_state(gst::State::Paused)
            .map_err(|error| Error::Backend(format!("cannot preroll the URL video: {error:?}")))?;
        Ok(player)
    }

    pub fn set_playing(&self, playing: bool) {
        let target = {
            let Ok(mut shared) = self.shared.lock() else {
                return;
            };
            shared.wanted_playing = playing;
            shared.target_state()
        };
        if let Err(error) = self.pipeline.set_state(target) {
            tracing::warn!(?error, "the URL video refused a play state change");
        }
    }

    pub fn seek(&self, seconds: f64) {
        let ready = self.shared.lock().is_ok_and(|shared| shared.ready);
        if !ready {
            if let Ok(mut shared) = self.shared.lock() {
                shared.pending_seek = Some(seconds);
            }
            return;
        }
        seek_pipeline(&self.pipeline, seconds);
    }

    pub fn set_muted(&self, muted: bool) {
        self.volume.set_property("mute", muted);
    }

    pub fn status(&self) -> UrlPlayerStatus {
        let position_seconds = self
            .pipeline
            .query_position::<gst::ClockTime>()
            .map(|position| position.nseconds() as f64 / 1e9);
        let duration_seconds = self
            .pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.nseconds() as f64 / 1e9);
        let playing = self.pipeline.current_state() == gst::State::Playing;
        let Ok(shared) = self.shared.lock() else {
            return UrlPlayerStatus {
                failed: true,
                ..UrlPlayerStatus::default()
            };
        };
        UrlPlayerStatus {
            ready: shared.ready,
            playing,
            ended: shared.ended,
            failed: shared.failed,
            position_seconds,
            duration_seconds,
            buffered_percent: shared.buffered_percent,
        }
    }
}

impl Drop for UrlVideoPlayer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(worker) = self.bus_worker.take() {
            let _ = worker.join();
        }
        let _ = self.display.unregister_video_source(self.display_source);
    }
}

fn video_branch(
    pipeline: &gst::Pipeline,
    offset: Duration,
) -> Result<(gst::Element, gst_app::AppSink)> {
    let queue = make("queue", "url-video-queue")?;
    set_checked(&queue, "max-size-time", VIDEO_QUEUE_NS)?;
    set_checked(&queue, "max-size-bytes", 0_u32)?;
    set_checked(&queue, "max-size-buffers", 0_u32)?;
    let scale = make("videoconvertscale", "url-video-scale")?;
    set_checked(&scale, "add-borders", true)?;
    set_checked(&scale, "n-threads", SCALER_THREADS)?;
    let caps = make("capsfilter", "url-video-caps")?;
    set_checked(
        &caps,
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .field("width", i32::from(crate::SCREEN_WIDTH))
            .field("height", i32::from(crate::SCREEN_HEIGHT))
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build(),
    )?;
    let sink = make("appsink", "url-video-sink")?;
    let appsink = sink
        .clone()
        .downcast::<gst_app::AppSink>()
        .map_err(|_| Error::Backend("URL video appsink has the wrong type".to_owned()))?;
    appsink.set_sync(true);
    appsink.set_drop(true);
    appsink.set_max_buffers(2);
    set_checked(
        &sink,
        "ts-offset",
        i64::try_from(offset.as_nanos()).unwrap_or(0),
    )?;
    let elements = [queue.clone(), scale, caps, sink];
    pipeline
        .add_many(&elements)
        .map_err(gst_error("cannot add the URL video branch"))?;
    gst::Element::link_many(&elements).map_err(gst_error("cannot link the URL video branch"))?;
    Ok((queue, appsink))
}

fn audio_branch(
    pipeline: &gst::Pipeline,
    audible: bool,
) -> Result<(gst::Element, gst::Element, Option<gst_app::AppSink>)> {
    let queue = make("queue", "url-audio-queue")?;
    let convert = make("audioconvert", "url-audio-convert")?;
    let resample = make("audioresample", "url-audio-resample")?;
    let volume = make("volume", "url-audio-volume")?;
    let caps = make("capsfilter", "url-audio-caps")?;
    set_checked(
        &caps,
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("rate", PCM_INPUT_SAMPLE_RATE as i32)
            .field("channels", PCM_INPUT_CHANNELS as i32)
            .field("layout", "interleaved")
            .build(),
    )?;
    let (sink, appsink) = if audible {
        let sink = make("appsink", "url-audio-sink")?;
        let appsink = sink
            .clone()
            .downcast::<gst_app::AppSink>()
            .map_err(|_| Error::Backend("URL audio appsink has the wrong type".to_owned()))?;
        appsink.set_sync(true);
        (sink, Some(appsink))
    } else {
        let sink = make("fakesink", "url-audio-sink")?;
        set_checked(&sink, "sync", true)?;
        (sink, None)
    };
    set_checked(&sink, "async", false)?;
    let elements = [queue.clone(), convert, resample, volume.clone(), caps, sink];
    pipeline
        .add_many(&elements)
        .map_err(gst_error("cannot add the URL audio branch"))?;
    gst::Element::link_many(&elements).map_err(gst_error("cannot link the URL audio branch"))?;
    Ok((queue, volume, appsink))
}

fn link_decoded_pads(
    pipeline: &gst::Pipeline,
    source: &gst::Element,
    video: gst::Element,
    audio: gst::Element,
) {
    let pipeline = pipeline.downgrade();
    source.connect_pad_added(move |_, pad| {
        let media = pad_media(pad);
        let branch = match media {
            "video" => Some(&video),
            "audio" => Some(&audio),
            _ => None,
        };
        let target = branch
            .and_then(|branch| branch.static_pad("sink"))
            .filter(|sink| !sink.is_linked());
        let linked = target.is_some_and(|sink| pad.link(&sink).is_ok());
        if linked {
            tracing::info!(media, "CarPlay video stream decoded");
            return;
        }
        let Some(pipeline) = pipeline.upgrade() else {
            return;
        };
        let Ok(discard) = gst::ElementFactory::make("fakesink").build() else {
            return;
        };
        discard.set_property("sync", false);
        if pipeline.add(&discard).is_ok() {
            let _ = discard.sync_state_with_parent();
            if let Some(sink) = discard.static_pad("sink") {
                let _ = pad.link(&sink);
            }
        }
    });
}

fn pin_hardware_decoder_io(pipeline: &gst::Pipeline) {
    pipeline.connect_deep_element_added(|_, _, element| {
        let is_v4l2_decoder = element.factory().is_some_and(|factory| {
            factory.name().starts_with("v4l2") && factory.name().ends_with("dec")
        });
        if !is_v4l2_decoder {
            return;
        }
        for property in ["output-io-mode", "capture-io-mode"] {
            if element.find_property(property).is_some() {
                element.set_property_from_str(property, "mmap");
            }
        }
    });
}

fn pad_media(pad: &gst::Pad) -> &'static str {
    let kind = pad.stream().map(|stream| stream.stream_type());
    let name = pad.name();
    let caps = pad
        .current_caps()
        .and_then(|caps| {
            caps.structure(0)
                .map(|structure| structure.name().to_string())
        })
        .unwrap_or_default();
    if kind.is_some_and(|kind| kind.contains(gst::StreamType::VIDEO))
        || name.starts_with("video")
        || caps.starts_with("video/")
    {
        "video"
    } else if kind.is_some_and(|kind| kind.contains(gst::StreamType::AUDIO))
        || name.starts_with("audio")
        || caps.starts_with("audio/")
    {
        "audio"
    } else {
        "other"
    }
}

fn install_video_renderer(
    appsink: &gst_app::AppSink,
    display: LinuxDisplayHandle,
    display_source: u64,
    counters: Arc<Counters>,
) {
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                match render(&sample, &display, display_source) {
                    Ok(()) => {
                        counters.frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        if counters.undrawable.fetch_add(1, Ordering::Relaxed) < 5 {
                            tracing::warn!(%error, "dropping an undrawable CarPlay video frame");
                        }
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

fn install_audio_forwarder(appsink: &gst_app::AppSink, audio: PcmInput, counters: Arc<Counters>) {
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let Some(buffer) = sample.buffer() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                let Ok(map) = buffer.map_readable() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                match audio.push(map.as_slice()) {
                    Ok(()) => {
                        counters.audio_buffers.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => tracing::debug!(%error, "dropping CarPlay video audio"),
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

fn render(sample: &gst::Sample, display: &LinuxDisplayHandle, display_source: u64) -> Result<()> {
    let buffer = sample
        .buffer()
        .ok_or_else(|| Error::Backend("URL video sample has no buffer".to_owned()))?;
    let caps = sample
        .caps()
        .ok_or_else(|| Error::Backend("URL video sample has no caps".to_owned()))?;
    let info = VideoInfo::from_caps(caps)
        .map_err(|error| Error::Backend(format!("URL video caps are invalid: {error}")))?;
    let width = usize::from(crate::SCREEN_WIDTH);
    let height = usize::from(crate::SCREEN_HEIGHT);
    if info.format() != gstreamer_video::VideoFormat::Nv12
        || info.width() as usize != width
        || info.height() as usize != height
    {
        return Err(Error::Backend(
            "URL video frame does not match the display geometry".to_owned(),
        ));
    }
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|error| Error::Backend(format!("cannot map a URL video frame: {error}")))?;
    let y_plane = frame
        .plane_data(0)
        .map_err(|error| Error::Backend(format!("cannot map the NV12 Y plane: {error}")))?;
    let uv_plane = frame
        .plane_data(1)
        .map_err(|error| Error::Backend(format!("cannot map the NV12 UV plane: {error}")))?;
    let strides = frame.plane_stride();
    let y_pitch = usize::try_from(strides[0])
        .map_err(|_| Error::Backend("NV12 Y stride is negative".to_owned()))?;
    let uv_pitch = usize::try_from(strides[1])
        .map_err(|_| Error::Backend("NV12 UV stride is negative".to_owned()))?;
    let mut packed = vec![0_u8; width * height * 3 / 2];
    for row in 0..height {
        packed[row * width..(row + 1) * width]
            .copy_from_slice(&y_plane[row * y_pitch..row * y_pitch + width]);
    }
    let uv_offset = width * height;
    for row in 0..height / 2 {
        packed[uv_offset + row * width..uv_offset + (row + 1) * width]
            .copy_from_slice(&uv_plane[row * uv_pitch..row * uv_pitch + width]);
    }
    display.submit_video_for_source(display_source, packed)
}

fn watch_bus(
    bus: &gst::Bus,
    pipeline: &gst::Pipeline,
    shared: &Mutex<Shared>,
    stopped: &AtomicBool,
    counters: &Counters,
) {
    use gst::MessageView;
    let mut next_status = Instant::now() + STATUS_INTERVAL;
    while !stopped.load(Ordering::Acquire) {
        if Instant::now() >= next_status {
            next_status += STATUS_INTERVAL;
            log_status(pipeline, shared, counters);
        }
        let Some(message) = bus.timed_pop(BUS_POLL) else {
            continue;
        };
        let Ok(mut state) = shared.lock() else {
            return;
        };
        match message.view() {
            MessageView::Error(error) => {
                state.failed = true;
                tracing::warn!(
                    error = %error.error(),
                    debug = ?error.debug(),
                    "CarPlay video playback failed"
                );
            }
            MessageView::Eos(_) => {
                state.ended = true;
                tracing::info!("CarPlay video reached its end");
            }
            MessageView::Buffering(buffering) => {
                let percent = u8::try_from(buffering.percent().clamp(0, 100)).unwrap_or(100);
                state.buffered_percent = Some(percent);
                let filling = percent < 100;
                if filling != state.filling {
                    state.filling = filling;
                    if state.wanted_playing {
                        let target = state.target_state();
                        drop(state);
                        tracing::info!(percent, ?target, "CarPlay video buffering changed");
                        pipeline.call_async(move |pipeline| {
                            let _ = pipeline.set_state(target);
                        });
                    }
                }
            }
            MessageView::AsyncDone(_) => {
                state.ready = true;
                if let Some(seconds) = state.pending_seek.take() {
                    drop(state);
                    pipeline.call_async(move |pipeline| {
                        if let Some(pipeline) = pipeline.downcast_ref::<gst::Pipeline>() {
                            seek_pipeline(pipeline, seconds);
                        }
                    });
                }
            }
            _ => {}
        }
    }
}

fn buffer_running_time(pad: &gst::Pad, info: &gst::PadProbeInfo<'_>) -> Option<gst::ClockTime> {
    let pts = info.buffer()?.pts()?;
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    event
        .segment()
        .downcast_ref::<gst::ClockTime>()?
        .to_running_time(pts)
}

fn clock_shift(first: gst::ClockTime, clock: Option<gst::ClockTime>) -> i64 {
    let ahead = i128::from(first.nseconds())
        - i128::from(clock.map_or(0, gst::ClockTime::nseconds))
        - i128::from(CLOCK_ALIGNMENT_MARGIN.nseconds());
    if ahead > i128::from(CLOCK_ALIGNMENT_THRESHOLD.nseconds()) {
        i64::try_from(-ahead).unwrap_or(i64::MIN)
    } else {
        0
    }
}

fn align_to_clock(pipeline: &gst::Pipeline, inputs: [&gst::Element; 2]) {
    let aligned = Arc::new(AtomicBool::new(false));
    for input in inputs {
        let Some(pad) = input.static_pad("sink") else {
            continue;
        };
        let pipeline = pipeline.downgrade();
        let aligned = Arc::clone(&aligned);
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if aligned.swap(true, Ordering::AcqRel) {
                return gst::PadProbeReturn::Remove;
            }
            let (Some(pipeline), Some(first)) =
                (pipeline.upgrade(), buffer_running_time(pad, info))
            else {
                return gst::PadProbeReturn::Remove;
            };
            let shift = clock_shift(first, pipeline.current_running_time());
            if shift != 0 {
                for name in ["url-video-sink", "url-audio-sink"] {
                    if let Some(sink) = pipeline.by_name(name) {
                        let offset = sink.property::<i64>("ts-offset");
                        sink.set_property("ts-offset", offset.saturating_add(shift));
                    }
                }
                tracing::info!(
                    shift_ms = shift / 1_000_000,
                    "moved CarPlay video timestamps onto the playback clock"
                );
            }
            gst::PadProbeReturn::Remove
        });
    }
}

fn count_decoded_video(queue: &gst::Element, counters: Arc<Counters>) {
    let Some(pad) = queue.static_pad("sink") else {
        return;
    };
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        counters.decoded.fetch_add(1, Ordering::Relaxed);
        let running = buffer_running_time(pad, info).map_or(u64::MAX, gst::ClockTime::mseconds);
        let _ = counters.first_running_ms.compare_exchange(
            0,
            running,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        counters.last_running_ms.store(running, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
}

fn log_status(pipeline: &gst::Pipeline, shared: &Mutex<Shared>, counters: &Counters) {
    let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
    let clock_running_ms = pipeline
        .current_running_time()
        .map(gst::ClockTime::mseconds);
    let queued = pipeline
        .by_name("url-video-queue")
        .map(|queue| queue.property::<u32>("current-level-buffers"));
    let position = pipeline
        .query_position::<gst::ClockTime>()
        .map(|position| position.seconds_f64());
    let Ok(state) = shared.lock() else {
        return;
    };
    tracing::info!(
        ?current,
        ?pending,
        wanted_playing = state.wanted_playing,
        filling = state.filling,
        buffered_percent = ?state.buffered_percent,
        ready = state.ready,
        failed = state.failed,
        position = ?position,
        frames = counters.frames.load(Ordering::Relaxed),
        undrawable = counters.undrawable.load(Ordering::Relaxed),
        audio_buffers = counters.audio_buffers.load(Ordering::Relaxed),
        decoded = counters.decoded.load(Ordering::Relaxed),
        first_running_ms = counters.first_running_ms.load(Ordering::Relaxed),
        last_running_ms = counters.last_running_ms.load(Ordering::Relaxed),
        clock_running_ms = ?clock_running_ms,
        queued = ?queued,
        "CarPlay video player status"
    );
}

fn seek_pipeline(pipeline: &gst::Pipeline, seconds: f64) {
    let target = gst::ClockTime::from_nseconds((seconds.max(0.0) * 1e9) as u64);
    if let Err(error) =
        pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target)
    {
        tracing::warn!(%error, seconds, "the URL video refused a seek");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        time::Instant,
    };

    use super::*;

    const REQUIRED: [&str; 9] = [
        "x264enc",
        "avenc_aac",
        "hlssink2",
        "uridecodebin",
        "souphttpsrc",
        "tsdemux",
        "h264parse",
        "videoconvertscale",
        "audiomixer",
    ];

    fn missing_elements() -> Vec<&'static str> {
        REQUIRED
            .into_iter()
            .filter(|factory| gst::ElementFactory::find(factory).is_none())
            .collect()
    }

    fn write_hls(directory: &Path, with_audio: bool) {
        let audio = if with_audio {
            "audiotestsrc num-buffers=250 samplesperbuffer=1024 ! audio/x-raw,rate=48000,channels=2 ! \
             avenc_aac ! aacparse ! hls.audio"
        } else {
            ""
        };
        let description = format!(
            "videotestsrc num-buffers=150 pattern=smpte ! video/x-raw,width=640,height=360,framerate=30/1 ! \
             x264enc tune=zerolatency key-int-max=30 bitrate=800 ! h264parse ! hls.video \
             {audio} \
             hlssink2 name=hls target-duration=1 playlist-length=0 max-files=0 \
             location={0}/segment%05d.ts playlist-location={0}/index.m3u8",
            directory.display()
        );
        let pipeline = gst::parse::launch(&description).expect("HLS writer");
        pipeline.set_state(gst::State::Playing).expect("write HLS");
        let bus = pipeline.bus().expect("bus");
        let message = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(60),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("HLS writer finished");
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "HLS writer failed: {message:?}"
        );
        pipeline.set_state(gst::State::Null).expect("stop writer");
    }

    fn serve(directory: PathBuf) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let directory = directory.clone();
                thread::spawn(move || {
                    let mut request = [0_u8; 4096];
                    let Ok(length) = stream.read(&mut request) else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&request[..length]);
                    let path = request.split(' ').nth(1).unwrap_or("/");
                    let file = directory.join(path.trim_start_matches('/'));
                    let Ok(body) = std::fs::read(&file) else {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    };
                    let kind = if path.ends_with(".m3u8") {
                        "application/vnd.apple.mpegurl"
                    } else {
                        "video/mp2t"
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                });
            }
        });
        port
    }

    fn mixer() -> gst::Pipeline {
        let pipeline = gst::parse::launch(
            "audiotestsrc is-live=true wave=silence ! audioconvert ! \
             audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved ! \
             audiomixer name=carplay-mixer ! \
             capsfilter name=float-mixer-output caps=audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved ! \
             fakesink sync=true",
        )
        .expect("mixer")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
        pipeline
            .set_state(gst::State::Playing)
            .expect("mixer plays");
        pipeline
    }

    #[test]
    fn plays_hls_into_the_display_and_the_mixer() {
        gst::init().expect("GStreamer");
        let missing = missing_elements();
        if !missing.is_empty() {
            eprintln!("SKIP URL video integration: missing {missing:?}");
            return;
        }
        let directory = tempfile::tempdir().expect("directory");
        write_hls(directory.path(), true);
        let port = serve(directory.path().to_path_buf());
        let mixer = mixer();
        let audio = PcmInput::attach(&mixer).expect("mixer input");
        let display = LinuxDisplayHandle::detached_for_test();
        let player = UrlVideoPlayer::start(
            &format!("http://127.0.0.1:{port}/index.m3u8"),
            2.0,
            false,
            display.clone(),
            Some(audio),
        )
        .expect("player");
        player.set_playing(true);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut status = player.status();
        while Instant::now() < deadline
            && (display.latest_video_for_test().is_none()
                || status.position_seconds.unwrap_or(0.0) < 1.0)
        {
            assert!(!status.failed, "playback failed");
            thread::sleep(Duration::from_millis(100));
            status = player.status();
        }
        let frame = display
            .latest_video_for_test()
            .expect("a frame reached the display");
        let width = usize::from(crate::SCREEN_WIDTH);
        assert_eq!(
            frame.len(),
            width * usize::from(crate::SCREEN_HEIGHT) * 3 / 2
        );
        assert!(
            frame[..width].iter().all(|luma| *luma <= 20),
            "top letterbox row is black"
        );
        assert!(status.ready);
        assert!(status.playing);
        assert!(status.position_seconds.unwrap_or(0.0) >= 1.0);
        assert!(status.duration_seconds.unwrap_or(0.0) >= 4.0);
        player.seek(3.0);
        drop(player);
        assert!(
            mixer.by_name("video-pcm-source").is_none(),
            "mixer input detached"
        );
        mixer.set_state(gst::State::Null).expect("stop mixer");
    }

    #[test]
    fn timestamps_far_ahead_of_the_clock_are_pulled_back() {
        let shift = clock_shift(
            gst::ClockTime::from_mseconds(780_201),
            Some(gst::ClockTime::from_mseconds(4_788)),
        );
        assert_eq!(shift, -(775_413 - 100) * 1_000_000);
        assert_eq!(
            clock_shift(
                gst::ClockTime::from_mseconds(5_500),
                Some(gst::ClockTime::from_mseconds(4_788))
            ),
            0
        );
        assert_eq!(clock_shift(gst::ClockTime::from_mseconds(200), None), 0);
    }

    #[test]
    fn plays_video_whose_audio_cannot_be_decoded() {
        gst::init().expect("GStreamer");
        let missing = missing_elements();
        if !missing.is_empty() {
            eprintln!("SKIP URL video integration: missing {missing:?}");
            return;
        }
        let directory = tempfile::tempdir().expect("directory");
        write_hls(directory.path(), false);
        let port = serve(directory.path().to_path_buf());
        let mixer = mixer();
        let audio = PcmInput::attach(&mixer).expect("mixer input");
        let display = LinuxDisplayHandle::detached_for_test();
        let player = UrlVideoPlayer::start(
            &format!("http://127.0.0.1:{port}/index.m3u8"),
            0.0,
            false,
            display.clone(),
            Some(audio),
        )
        .expect("player");
        player.set_playing(true);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut status = player.status();
        while Instant::now() < deadline && status.position_seconds.unwrap_or(0.0) < 1.0 {
            assert!(!status.failed, "playback failed");
            thread::sleep(Duration::from_millis(100));
            status = player.status();
        }
        assert!(status.ready, "the pipeline prerolled");
        assert!(
            status.position_seconds.unwrap_or(0.0) >= 1.0,
            "playback advanced"
        );
        assert!(display.latest_video_for_test().is_some());
        drop(player);
        mixer.set_state(gst::State::Null).expect("stop mixer");
    }
}
