use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use prost::Message;

use crate::{
    Error, Result,
    constants::{
        FLAG_ENCRYPTED, FLAG_ENCRYPTED_CONTROL, FLAG_ENCRYPTED_CONTROL_BASE, FLAG_ENCRYPTED_SIGNAL,
        FLAG_ENCRYPTED_SPECIFIC_BASE, FLAG_PLAINTEXT, FRAME_TYPE_BULK, FRAME_TYPE_FIRST,
        FRAME_TYPE_LAST, FRAME_TYPE_MASK, FRAME_TYPE_MIDDLE, MAX_MIC_BACKLOG, MAX_MIC_PCM,
        MAX_REASSEMBLED_PAYLOAD, MAX_TOUCH_POINTERS, MEDIA_CODEC_H264_BASELINE, MEDIA_CODEC_PCM,
        STATUS_OK, VERSION_MAJOR, VERSION_MINOR, channel, message,
    },
    frame::{
        FrameDecoder, MessageAssembler, PhysicalFrame, encode_encrypted_frame, encode_plain_frame,
    },
    proto::{
        Ack, AudioFocusRequest, BatteryStatusNotification, BluetoothAuthenticationResult,
        BluetoothPairingRequest, ChannelOpenRequest, MicrophoneRequest, MicrophoneResponse, Ping,
        SensorRequest, ServiceDiscoveryRequest, Setup, ShutdownRequest, Start, TouchPointMessage,
        VideoFocusNotification, VoiceSessionNotification, ack, auth_complete,
        bluetooth_authentication, bluetooth_pairing_response, decode, input_key, input_rotary,
        input_touch, service_discovery_payload_with_density, setup_ok, setup_ok_with_window,
        status_ok, validate_bluetooth_address, validate_head_unit_name,
    },
    tls::{AapTls, split_tls_records},
};

use crate::proto::{DEFAULT_VIDEO_DENSITY_DPI, MAX_VIDEO_DENSITY_DPI, MIN_VIDEO_DENSITY_DPI};

const VIDEO_WIDTH: u32 = 800;
const VIDEO_HEIGHT: u32 = 480;
const MICROPHONE_SESSION_ID: i32 = 1;
const MAX_MIC_WINDOW: usize = 16;
const AUDIO_MAX_UNACKED: u32 = 4;
/// How many video frames the phone may send before one of them has been
/// acknowledged. With TCP_QUICKACK enabled, ACKs return immediately without
/// delayed-ACK stall, and 6 frames provide ~100ms credit buffer at 60fps (or
/// ~200ms at 30fps) to absorb Wi-Fi packet jitter without buffering lag or
/// triggering TCP buffer-bloat retransmission cascades.
const VIDEO_MAX_UNACKED: u32 = 6;
const READ_BUFFER_LIMIT: usize = 256 * 1024;
const MAX_ACTIVE_TLS_REASSEMBLIES: usize = 16;
const MAX_PHONE_ADDRESS_BYTES: usize = 32;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub head_unit_name: String,
    pub video_density_dpi: u32,
    pub read_buffer_size: usize,
    pub pre_running_timeout: Duration,
    pub ping_interval: Duration,
    pub ping_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub initial_night_mode: bool,
    pub audio_output: bool,
    pub bluetooth_address: Option<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            head_unit_name: "Prelude".to_owned(),
            video_density_dpi: DEFAULT_VIDEO_DENSITY_DPI,
            read_buffer_size: 128 * 1024,
            pre_running_timeout: Duration::from_secs(30),
            ping_interval: Duration::from_millis(1_500),
            ping_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(1),
            initial_night_mode: false,
            audio_output: true,
            bluetooth_address: None,
        }
    }
}

impl SessionConfig {
    fn validate(&self) -> Result<()> {
        validate_head_unit_name(&self.head_unit_name)?;
        if let Some(address) = &self.bluetooth_address {
            validate_bluetooth_address(address)?;
        }
        if !(MIN_VIDEO_DENSITY_DPI..=MAX_VIDEO_DENSITY_DPI).contains(&self.video_density_dpi) {
            return Err(Error::InvalidMessage(
                "video density is outside the supported range",
            ));
        }
        if self.read_buffer_size == 0 || self.read_buffer_size > READ_BUFFER_LIMIT {
            return Err(Error::InvalidMessage(
                "read buffer size is outside the supported range",
            ));
        }
        if !(Duration::from_secs(1)..=Duration::from_secs(60)).contains(&self.pre_running_timeout) {
            return Err(Error::InvalidMessage(
                "pre-running timeout is outside the supported range",
            ));
        }
        if !(Duration::from_millis(250)..=Duration::from_secs(5)).contains(&self.ping_interval)
            || self.ping_timeout <= self.ping_interval
            || self.ping_timeout > Duration::from_secs(30)
        {
            return Err(Error::InvalidMessage("ping timing is invalid"));
        }
        if !(Duration::from_millis(100)..=Duration::from_secs(5)).contains(&self.shutdown_timeout) {
            return Err(Error::InvalidMessage(
                "shutdown timeout is outside the supported range",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionState {
    Created,
    Version,
    TlsHandshake,
    Auth,
    ServiceDiscovery,
    ChannelSetup,
    Running,
    Closing,
    Closed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionTrace {
    pub closed_in: Option<SessionState>,
    pub last_message: Option<(u8, u16)>,
    pub opened_channels: Vec<u8>,
}

/// Measurements of the transport link since the window was last taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkWindow {
    /// Transport reads that returned data.
    pub reads: u64,
    pub read_bytes: u64,
    /// Transport reads that ended at their timeout with no data.
    pub read_timeouts: u64,
    /// Pings the phone answered, and how long each answer took.
    pub ping_round_trips: u64,
    pub ping_total_micros: u64,
    pub ping_minimum_micros: u64,
    pub ping_maximum_micros: u64,
    /// Video frames received, the reads each one needed, and the time from
    /// the first read holding part of a frame to the frame being complete.
    pub video_frames: u64,
    pub video_reads_total: u64,
    pub video_reads_maximum: u64,
    pub video_assembly_total_micros: u64,
    pub video_assembly_maximum_micros: u64,
    /// The round trip of each answered ping in the order the answers came,
    /// for the first [`LINK_SAMPLES`] of them.
    pub ping_round_trip_micros: [u64; LINK_SAMPLES],
    /// Pings the phone sent.
    pub phone_pings: u64,
    /// How many of the phone's pings have a gap recorded below.
    pub phone_ping_gaps: u64,
    /// For each ping the phone sent after its first: how long after the
    /// previous one it arrived here, and how far the timestamp the phone
    /// wrote into it had advanced, in the phone's own unit. The phone sends
    /// its pings without waiting for anything from this side, so where the
    /// two disagree the ping was held up on its way here.
    pub phone_ping_arrival_gap_micros: [u64; LINK_SAMPLES],
    pub phone_ping_timestamp_gap: [i64; LINK_SAMPLES],
}

/// How many individual pings of each direction a window keeps.
pub const LINK_SAMPLES: usize = 8;

impl LinkWindow {
    fn record_phone_ping(&mut self, gap: Option<(Duration, i64)>) {
        self.phone_pings = self.phone_pings.saturating_add(1);
        let Some((arrival_gap, timestamp_gap)) = gap else {
            return;
        };
        let index = usize::try_from(self.phone_ping_gaps).unwrap_or(usize::MAX);
        if let (Some(arrival), Some(timestamp)) = (
            self.phone_ping_arrival_gap_micros.get_mut(index),
            self.phone_ping_timestamp_gap.get_mut(index),
        ) {
            *arrival = u64::try_from(arrival_gap.as_micros()).unwrap_or(u64::MAX);
            *timestamp = timestamp_gap;
            self.phone_ping_gaps = self.phone_ping_gaps.saturating_add(1);
        }
    }

    fn record_ping(&mut self, round_trip: Duration) {
        let micros = u64::try_from(round_trip.as_micros()).unwrap_or(u64::MAX);
        let index = usize::try_from(self.ping_round_trips).unwrap_or(usize::MAX);
        if let Some(sample) = self.ping_round_trip_micros.get_mut(index) {
            *sample = micros;
        }
        self.ping_minimum_micros = if self.ping_round_trips == 0 {
            micros
        } else {
            self.ping_minimum_micros.min(micros)
        };
        self.ping_maximum_micros = self.ping_maximum_micros.max(micros);
        self.ping_total_micros = self.ping_total_micros.saturating_add(micros);
        self.ping_round_trips = self.ping_round_trips.saturating_add(1);
    }

    fn record_video(&mut self, assembly: Duration, reads: u64) {
        let micros = u64::try_from(assembly.as_micros()).unwrap_or(u64::MAX);
        self.video_frames = self.video_frames.saturating_add(1);
        self.video_reads_total = self.video_reads_total.saturating_add(reads);
        self.video_reads_maximum = self.video_reads_maximum.max(reads);
        self.video_assembly_total_micros = self.video_assembly_total_micros.saturating_add(micros);
        self.video_assembly_maximum_micros = self.video_assembly_maximum_micros.max(micros);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioChannel {
    Media,
    Guidance,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    PeerClosed,
    PeerShutdown,
    LocalShutdown,
    Cancelled,
    PingTimeout,
    StartupTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connected,
    Disconnected {
        reason: DisconnectReason,
    },
    Video {
        timestamp: Option<u64>,
        annex_b: Vec<u8>,
    },
    Audio {
        channel: AudioChannel,
        timestamp: Option<u64>,
        pcm_s16le: Vec<u8>,
    },
    Microphone {
        open: bool,
        sample_rate: u32,
        channels: u8,
    },
    VoiceSession {
        active: bool,
    },
    Battery {
        level: u32,
        critical: bool,
        time_remaining_seconds: Option<u32>,
    },
    BluetoothPairing {
        phone_address: Option<String>,
    },
    BluetoothAuthenticated {
        status: Option<i32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum TouchAction {
    Down = 0,
    Up = 1,
    Moved = 2,
    PointerDown = 5,
    PointerUp = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchPointer {
    pub x: u32,
    pub y: u32,
    pub id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Home,
    Back,
    PhoneAccept,
    PhoneDecline,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    DpadCenter,
    Enter,
    HeadsetHook,
    Menu,
    Search,
    Mute,
    Escape,
    Assist,
    VoiceAssist,
    NavigatePrevious,
    NavigateNext,
    NavigateIn,
    NavigateOut,
}

impl Button {
    const fn keycode(self) -> u32 {
        match self {
            Self::Home => 3,
            Self::Back => 4,
            Self::PhoneAccept => 5,
            Self::PhoneDecline => 6,
            Self::DpadUp => 19,
            Self::DpadDown => 20,
            Self::DpadLeft => 21,
            Self::DpadRight => 22,
            Self::DpadCenter => 23,
            Self::Enter => 66,
            Self::HeadsetHook => 79,
            Self::Menu => 82,
            Self::Search => 84,
            Self::Mute => 91,
            Self::Escape => 111,
            Self::Assist => 219,
            Self::VoiceAssist => 231,
            Self::NavigatePrevious => 260,
            Self::NavigateNext => 261,
            Self::NavigateIn => 262,
            Self::NavigateOut => 263,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKey {
    PlayPause,
    Stop,
    Next,
    Previous,
    Rewind,
    FastForward,
    Play,
    Pause,
}

impl MediaKey {
    const fn keycode(self) -> u32 {
        match self {
            Self::PlayPause => 85,
            Self::Stop => 86,
            Self::Next => 87,
            Self::Previous => 88,
            Self::Rewind => 89,
            Self::FastForward => 90,
            Self::Play => 126,
            Self::Pause => 127,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Touch {
        timestamp_micros: u64,
        action: TouchAction,
        action_index: u32,
        pointers: Vec<TouchPointer>,
    },
    Button {
        timestamp_micros: u64,
        button: Button,
        down: bool,
        longpress: bool,
    },
    MediaKey {
        timestamp_micros: u64,
        key: MediaKey,
        down: bool,
    },
    Rotary {
        timestamp_micros: u64,
        direction: i8,
    },
    MicrophonePcm {
        timestamp: u64,
        pcm_s16le: Vec<u8>,
    },
    RequestKeyFrame,
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct TlsFragment {
    base_flags: u8,
    expected: usize,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct TlsCleartextAssembler {
    channels: BTreeMap<u8, TlsFragment>,
}

impl TlsCleartextAssembler {
    fn push(
        &mut self,
        channel_id: u8,
        flags: u8,
        announced_total: Option<usize>,
        plaintext: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let frame_type = flags & FRAME_TYPE_MASK;
        let base_flags = flags & !FRAME_TYPE_MASK;
        if !matches!(
            base_flags,
            FLAG_ENCRYPTED_SPECIFIC_BASE | FLAG_ENCRYPTED_CONTROL_BASE
        ) {
            return Err(Error::InvalidFrame("unsupported encrypted message flags"));
        }
        match frame_type {
            FRAME_TYPE_BULK => {
                if announced_total.is_some() || self.channels.contains_key(&channel_id) {
                    return Err(Error::InvalidFrame(
                        "encrypted bulk frame overlaps fragmented message",
                    ));
                }
                if plaintext.len() > MAX_REASSEMBLED_PAYLOAD {
                    return Err(Error::Oversized {
                        kind: "TLS cleartext message",
                        actual: plaintext.len(),
                        limit: MAX_REASSEMBLED_PAYLOAD,
                    });
                }
                Ok(Some(plaintext.to_vec()))
            }
            FRAME_TYPE_FIRST => {
                let expected = announced_total.ok_or(Error::InvalidFrame(
                    "encrypted first fragment omits its announced total",
                ))?;
                if expected <= plaintext.len() || expected > MAX_REASSEMBLED_PAYLOAD {
                    return Err(Error::InvalidFrame(
                        "encrypted first fragment has an invalid announced total",
                    ));
                }
                if self.channels.contains_key(&channel_id) {
                    return Err(Error::InvalidFrame(
                        "encrypted first fragment overlaps active message",
                    ));
                }
                if self.channels.len() >= MAX_ACTIVE_TLS_REASSEMBLIES {
                    return Err(Error::InvalidFrame(
                        "too many active encrypted fragmented messages",
                    ));
                }
                self.channels.insert(
                    channel_id,
                    TlsFragment {
                        base_flags,
                        expected,
                        bytes: plaintext.to_vec(),
                    },
                );
                Ok(None)
            }
            FRAME_TYPE_MIDDLE | FRAME_TYPE_LAST => {
                if announced_total.is_some() {
                    return Err(Error::InvalidFrame(
                        "encrypted continuation has an extended total",
                    ));
                }
                let state = self
                    .channels
                    .get_mut(&channel_id)
                    .ok_or(Error::InvalidFrame(
                        "encrypted continuation has no first fragment",
                    ))?;
                if state.base_flags != base_flags {
                    self.channels.remove(&channel_id);
                    return Err(Error::InvalidFrame(
                        "encrypted fragment changes its message-type flags",
                    ));
                }
                let next = state
                    .bytes
                    .len()
                    .checked_add(plaintext.len())
                    .ok_or(Error::InvalidFrame("TLS cleartext length overflow"))?;
                if next > state.expected || next > MAX_REASSEMBLED_PAYLOAD {
                    self.channels.remove(&channel_id);
                    return Err(Error::InvalidFrame(
                        "TLS cleartext exceeds its announced total",
                    ));
                }
                state.bytes.extend_from_slice(plaintext);
                if frame_type == FRAME_TYPE_MIDDLE {
                    if next == state.expected {
                        self.channels.remove(&channel_id);
                        return Err(Error::InvalidFrame(
                            "encrypted middle fragment completes the message",
                        ));
                    }
                    return Ok(None);
                }
                let state = self
                    .channels
                    .remove(&channel_id)
                    .ok_or(Error::InvalidFrame("encrypted fragment state disappeared"))?;
                if state.bytes.len() != state.expected {
                    return Err(Error::InvalidFrame(
                        "TLS cleartext does not match its announced total",
                    ));
                }
                Ok(Some(state.bytes))
            }
            _ => Err(Error::InvalidFrame("invalid encrypted frame type")),
        }
    }
}

#[derive(Debug)]
struct QueuedMicrophoneFrame {
    timestamp: u64,
    pcm: Vec<u8>,
}

pub struct Session<T> {
    transport: T,
    config: SessionConfig,
    state: SessionState,
    decoder: FrameDecoder,
    plaintext: MessageAssembler,
    tls_cleartext: TlsCleartextAssembler,
    tls: Option<AapTls>,
    started_at: Option<Instant>,
    last_inbound: Option<Instant>,
    last_ping: Option<Instant>,
    shutdown_deadline: Option<Instant>,
    keyframe_deadline: Option<Instant>,
    sessions: BTreeMap<u8, i32>,
    microphone_open: bool,
    microphone_max_unacked: usize,
    microphone_unacked: usize,
    microphone_queue: VecDeque<QueuedMicrophoneFrame>,
    night_mode: Option<bool>,
    bluetooth_pairing_pending: bool,
    disconnected_emitted: bool,
    trace: SessionTrace,
    link: LinkWindow,
    /// The timestamp of the ping awaiting its answer, and when it was written.
    ping_sent: Option<(i64, Instant)>,
    /// The timestamp inside the phone's latest ping, and when it arrived.
    phone_ping: Option<(i64, Instant)>,
    /// When the first read holding part of the message now being assembled
    /// returned, and how many reads that message has taken so far.
    receive_started: Option<(Instant, u64)>,
    read_buffer: Vec<u8>,
}

impl<T> std::fmt::Debug for Session<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("state", &self.state)
            .field("microphone_open", &self.microphone_open)
            .field("microphone_unacked", &self.microphone_unacked)
            .field("microphone_queue_len", &self.microphone_queue.len())
            .field("night_mode", &self.night_mode)
            .finish_non_exhaustive()
    }
}

impl<T: Read + Write + Send> Session<T> {
    pub fn new(transport: T, config: SessionConfig) -> Result<Self> {
        config.validate()?;
        let read_buffer = vec![0_u8; config.read_buffer_size];
        Ok(Self {
            transport,
            config,
            state: SessionState::Created,
            decoder: FrameDecoder::new(),
            plaintext: MessageAssembler::default(),
            tls_cleartext: TlsCleartextAssembler::default(),
            tls: None,
            started_at: None,
            last_inbound: None,
            last_ping: None,
            shutdown_deadline: None,
            keyframe_deadline: None,
            sessions: BTreeMap::new(),
            microphone_open: false,
            microphone_max_unacked: 1,
            microphone_unacked: 0,
            microphone_queue: VecDeque::new(),
            night_mode: None,
            bluetooth_pairing_pending: false,
            disconnected_emitted: false,
            trace: SessionTrace::default(),
            link: LinkWindow::default(),
            ping_sent: None,
            phone_ping: None,
            receive_started: None,
            read_buffer,
        })
    }

    /// Returns the link measurements since the last call and starts a new
    /// window.
    pub fn take_link_window(&mut self) -> LinkWindow {
        std::mem::take(&mut self.link)
    }

    pub fn start(&mut self) -> Result<()> {
        self.start_at(Instant::now())
    }

    pub fn start_at(&mut self, now: Instant) -> Result<()> {
        if self.state != SessionState::Created {
            return Err(Error::InvalidState {
                operation: "starting the session more than once",
            });
        }
        let mut version = Vec::with_capacity(4);
        version.extend_from_slice(&VERSION_MAJOR.to_be_bytes());
        version.extend_from_slice(&VERSION_MINOR.to_be_bytes());
        self.write_plain(channel::CONTROL, message::VERSION_REQUEST, &version)?;
        self.started_at = Some(now);
        self.last_inbound = Some(now);
        self.state = SessionState::Version;
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Vec<Event>> {
        self.poll_at(Instant::now())
    }

    pub fn poll_at(&mut self, now: Instant) -> Result<Vec<Event>> {
        if self.state == SessionState::Created {
            return Err(Error::InvalidState {
                operation: "polling before session start",
            });
        }
        if self.state == SessionState::Closed {
            return Ok(Vec::new());
        }
        let mut buffer = std::mem::take(&mut self.read_buffer);
        if buffer.len() != self.config.read_buffer_size {
            buffer.resize(self.config.read_buffer_size, 0);
        }
        let result = match self.transport.read(&mut buffer) {
            Ok(0) => Ok(self.close_with(DisconnectReason::PeerClosed)),
            Ok(read) => {
                let received_at = Instant::now();
                self.link.reads = self.link.reads.saturating_add(1);
                self.link.read_bytes = self.link.read_bytes.saturating_add(read as u64);
                match self.receive_started.as_mut() {
                    Some((_, reads)) => *reads = reads.saturating_add(1),
                    None => self.receive_started = Some((received_at, 1)),
                }
                match self.ingest_at(&buffer[..read], now) {
                    Ok(events) => {
                        if self.receive_is_idle() {
                            self.receive_started = None;
                        } else if self.receive_started.is_none() {
                            // A message completed in this read and the bytes left
                            // over belong to the next one.
                            self.receive_started = Some((received_at, 1));
                        }
                        Ok(events)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                self.link.read_timeouts = self.link.read_timeouts.saturating_add(1);
                self.tick(now)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => self.tick(now),
            Err(error) => Err(Error::Io(error)),
        };
        self.read_buffer = buffer;
        result
    }

    pub fn ingest_at(&mut self, bytes: &[u8], now: Instant) -> Result<Vec<Event>> {
        if matches!(self.state, SessionState::Created | SessionState::Closed) {
            return Err(Error::InvalidState {
                operation: "ingesting transport data in an inactive session",
            });
        }
        let frames = self.decoder.push(bytes)?;
        let mut events = Vec::new();
        for frame in frames {
            self.last_inbound = Some(now);
            events.extend(self.handle_physical_frame(frame, now)?);
            if self.state == SessionState::Closed {
                break;
            }
        }
        events.extend(self.tick(now)?);
        Ok(events)
    }

    /// Whether no partially received message is waiting for more bytes.
    fn receive_is_idle(&self) -> bool {
        self.decoder.buffered_len() == 0
            && self.plaintext.is_idle()
            && self.tls_cleartext.channels.is_empty()
    }

    pub fn tick(&mut self, now: Instant) -> Result<Vec<Event>> {
        if self.state == SessionState::Closed {
            return Ok(Vec::new());
        }
        if self.state == SessionState::Closing {
            if self
                .shutdown_deadline
                .is_some_and(|deadline| now >= deadline)
            {
                return Ok(self.close_with(DisconnectReason::LocalShutdown));
            }
            return Ok(Vec::new());
        }
        if matches!(
            self.state,
            SessionState::Version
                | SessionState::TlsHandshake
                | SessionState::Auth
                | SessionState::ServiceDiscovery
                | SessionState::ChannelSetup
        ) && self.started_at.is_some_and(|started| {
            now.saturating_duration_since(started) > self.config.pre_running_timeout
        }) {
            return Ok(self.close_with(DisconnectReason::StartupTimeout));
        }
        if matches!(
            self.state,
            SessionState::ChannelSetup | SessionState::Running
        ) {
            if self
                .keyframe_deadline
                .is_some_and(|deadline| now >= deadline)
            {
                self.keyframe_deadline = None;
                self.send_encrypted(
                    channel::VIDEO,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::VIDEO_FOCUS_INDICATION,
                    &[0x08, 0x01],
                )?;
            }
            if self
                .last_inbound
                .is_some_and(|last| now.saturating_duration_since(last) > self.config.ping_timeout)
            {
                return Ok(self.close_with(DisconnectReason::PingTimeout));
            }
            if self
                .last_ping
                .is_none_or(|last| now.saturating_duration_since(last) >= self.config.ping_interval)
            {
                self.send_ping(now)?;
            }
        }
        Ok(Vec::new())
    }

    pub fn send(&mut self, command: Command) -> Result<()> {
        self.send_at(command, Instant::now())
    }

    pub fn night_mode(&self) -> Option<bool> {
        self.night_mode
    }

    pub fn set_night_mode(&mut self, enabled: bool) -> Result<bool> {
        if self.night_mode.is_none() {
            return Ok(false);
        }
        if self.night_mode == Some(enabled) {
            return Ok(true);
        }
        self.send_encrypted(
            channel::SENSOR,
            FLAG_ENCRYPTED_SIGNAL,
            message::SENSOR_BATCH,
            &[0x52, 0x02, 0x08, u8::from(enabled)],
        )?;
        self.night_mode = Some(enabled);
        Ok(true)
    }

    pub fn answer_bluetooth_pairing(&mut self, already_paired: bool) -> Result<()> {
        if !self.bluetooth_pairing_pending {
            return Err(Error::InvalidState {
                operation: "answering a Bluetooth pairing request the phone never sent",
            });
        }
        self.bluetooth_pairing_pending = false;
        self.send_encrypted(
            channel::BLUETOOTH,
            FLAG_ENCRYPTED_SIGNAL,
            message::BLUETOOTH_PAIRING_RESPONSE,
            &bluetooth_pairing_response(already_paired),
        )
    }

    pub fn send_bluetooth_passkey(&mut self, passkey: u32) -> Result<()> {
        if self.config.bluetooth_address.is_none() {
            return Err(Error::InvalidState {
                operation: "sending a Bluetooth passkey without advertising Bluetooth",
            });
        }
        self.send_encrypted(
            channel::BLUETOOTH,
            FLAG_ENCRYPTED_SIGNAL,
            message::BLUETOOTH_AUTHENTICATION_DATA,
            &bluetooth_authentication(passkey),
        )
    }

    pub fn send_at(&mut self, command: Command, now: Instant) -> Result<()> {
        if self.state != SessionState::Running {
            return Err(Error::InvalidState {
                operation: "sending an input command before the session is running",
            });
        }
        match command {
            Command::Touch {
                timestamp_micros,
                action,
                action_index,
                pointers,
            } => self.send_touch(timestamp_micros, action, action_index, &pointers),
            Command::Button {
                timestamp_micros,
                button,
                down,
                longpress,
            } => self.send_encrypted(
                channel::INPUT,
                FLAG_ENCRYPTED_SIGNAL,
                message::INPUT_REPORT,
                &input_key(timestamp_micros, button.keycode(), down, longpress),
            ),
            Command::MediaKey {
                timestamp_micros,
                key,
                down,
            } => self.send_encrypted(
                channel::INPUT,
                FLAG_ENCRYPTED_SIGNAL,
                message::INPUT_REPORT,
                &input_key(timestamp_micros, key.keycode(), down, false),
            ),
            Command::Rotary {
                timestamp_micros,
                direction,
            } => {
                if !matches!(direction, -1 | 1) {
                    return Err(Error::InvalidMessage("rotary direction must be -1 or 1"));
                }
                self.send_encrypted(
                    channel::INPUT,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::INPUT_REPORT,
                    &input_rotary(timestamp_micros, i32::from(direction)),
                )
            }
            Command::MicrophonePcm {
                timestamp,
                pcm_s16le,
            } => self.queue_microphone(timestamp, pcm_s16le),
            Command::RequestKeyFrame => {
                self.send_encrypted(
                    channel::VIDEO,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::VIDEO_FOCUS_INDICATION,
                    &[0x08, 0x02],
                )?;
                self.keyframe_deadline = now.checked_add(Duration::from_millis(60));
                Ok(())
            }
        }
    }

    pub fn request_shutdown(&mut self, reason: i32) -> Result<()> {
        self.request_shutdown_at(reason, Instant::now())
    }

    pub fn request_shutdown_at(&mut self, reason: i32, now: Instant) -> Result<()> {
        if matches!(self.state, SessionState::Created | SessionState::Closed) {
            return Err(Error::InvalidState {
                operation: "requesting shutdown on an inactive session",
            });
        }
        if self.state == SessionState::Closing {
            return Ok(());
        }
        if self.tls.is_some() && self.state >= SessionState::Auth {
            let payload = ShutdownRequest {
                reason: Some(reason),
            }
            .encode_to_vec();
            self.send_encrypted(
                channel::CONTROL,
                FLAG_ENCRYPTED_SIGNAL,
                message::SHUTDOWN_REQUEST,
                &payload,
            )?;
            self.state = SessionState::Closing;
            self.shutdown_deadline = now.checked_add(self.config.shutdown_timeout);
            Ok(())
        } else {
            self.state = SessionState::Closed;
            Ok(())
        }
    }

    pub fn cancel_at(&mut self, now: Instant) -> Result<Vec<Event>> {
        if self.state == SessionState::Closed {
            return Ok(Vec::new());
        }
        if self.tls.is_some() && self.state >= SessionState::Auth {
            self.request_shutdown_at(1, now)?;
            Ok(Vec::new())
        } else {
            Ok(self.close_with(DisconnectReason::Cancelled))
        }
    }

    pub fn run(
        &mut self,
        cancellation: &CancellationToken,
        mut emit: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        if self.state == SessionState::Created {
            self.start()?;
        }
        while self.state != SessionState::Closed {
            if cancellation.is_cancelled() && self.state != SessionState::Closing {
                for event in self.cancel_at(Instant::now())? {
                    emit(event)?;
                }
                if self.state == SessionState::Closed {
                    break;
                }
            }
            for event in self.poll()? {
                emit(event)?;
            }
        }
        Ok(())
    }

    pub const fn state(&self) -> SessionState {
        self.state
    }

    pub const fn trace(&self) -> &SessionTrace {
        &self.trace
    }

    pub fn millis_since_last_inbound(&self, now: Instant) -> Option<u128> {
        self.last_inbound
            .map(|last| now.saturating_duration_since(last).as_millis())
    }

    pub const fn is_alive(&self) -> bool {
        !matches!(self.state, SessionState::Created | SessionState::Closed)
    }

    pub fn into_inner(self) -> T {
        self.transport
    }

    fn handle_physical_frame(&mut self, frame: PhysicalFrame, now: Instant) -> Result<Vec<Event>> {
        if frame.flags & FLAG_ENCRYPTED != 0 {
            return self.handle_encrypted_frame(frame, now);
        }
        let Some((_flags, message)) = self.plaintext.push(
            frame.channel_id,
            frame.flags,
            frame.announced_total,
            &frame.payload,
        )?
        else {
            return Ok(Vec::new());
        };
        let (message_id, payload) = split_message(&message)?;
        self.handle_plain_message(frame.channel_id, message_id, payload, now)
    }

    fn handle_plain_message(
        &mut self,
        channel_id: u8,
        message_id: u16,
        payload: &[u8],
        now: Instant,
    ) -> Result<Vec<Event>> {
        if channel_id != channel::CONTROL {
            return Err(Error::InvalidMessage(
                "plaintext message is not on control channel",
            ));
        }
        match self.state {
            SessionState::Version if message_id == message::VERSION_RESPONSE => {
                self.handle_version_response(payload)?;
                Ok(Vec::new())
            }
            SessionState::TlsHandshake if message_id == message::TLS_HANDSHAKE => {
                self.handle_tls_handshake(payload)
            }
            SessionState::ServiceDiscovery
            | SessionState::ChannelSetup
            | SessionState::Running
            | SessionState::Closing => self.handle_message(channel_id, message_id, payload, now),
            _ => Err(Error::InvalidMessage(
                "plaintext message is out of order for the session state",
            )),
        }
    }

    fn handle_version_response(&mut self, payload: &[u8]) -> Result<()> {
        let [m0, m1, n0, n1, s0, s1, ..] = *payload else {
            return Err(Error::VersionResponseTruncated {
                actual: payload.len(),
            });
        };
        let major = u16::from_be_bytes([m0, m1]);
        let minor = u16::from_be_bytes([n0, n1]);
        let status = u16::from_be_bytes([s0, s1]);
        if major != VERSION_MAJOR || minor != VERSION_MINOR || status != 0 {
            return Err(Error::VersionRejected {
                major,
                minor,
                status,
            });
        }
        let mut tls = AapTls::new()?;
        let client_hello = tls.start()?;
        if client_hello.is_empty() {
            return Err(Error::Tls("TLS client produced no ClientHello"));
        }
        self.write_plain(channel::CONTROL, message::TLS_HANDSHAKE, &client_hello)?;
        self.tls = Some(tls);
        self.state = SessionState::TlsHandshake;
        Ok(())
    }

    fn handle_tls_handshake(&mut self, payload: &[u8]) -> Result<Vec<Event>> {
        let progress = self
            .tls
            .as_mut()
            .ok_or(Error::Tls("TLS handshake state is missing"))?
            .inject(payload)?;
        if !progress.plaintext.is_empty() {
            return Err(Error::Tls(
                "application data arrived inside a handshake frame",
            ));
        }
        if !progress.outbound_tls.is_empty() {
            self.write_plain(
                channel::CONTROL,
                message::TLS_HANDSHAKE,
                &progress.outbound_tls,
            )?;
        }
        if progress.peer_closed {
            return Ok(self.close_with(DisconnectReason::PeerClosed));
        }
        if progress.handshake_complete {
            self.state = SessionState::Auth;
            self.write_plain(channel::CONTROL, message::AUTH_COMPLETE, &auth_complete())?;
            self.state = SessionState::ServiceDiscovery;
        }
        Ok(Vec::new())
    }

    fn handle_encrypted_frame(&mut self, frame: PhysicalFrame, now: Instant) -> Result<Vec<Event>> {
        if self.state < SessionState::ServiceDiscovery {
            return Err(Error::InvalidMessage(
                "encrypted message arrived before authentication",
            ));
        }
        let progress = self
            .tls
            .as_mut()
            .ok_or(Error::Tls("encrypted session has no TLS state"))?
            .inject(&frame.payload)?;
        if progress.handshake_complete {
            return Err(Error::Tls("TLS handshake completed more than once"));
        }
        if !progress.outbound_tls.is_empty() {
            let records = split_tls_records(&progress.outbound_tls)?;
            self.write_tls_records(frame.channel_id, frame.flags, 0, &records)?;
        }
        if progress.peer_closed {
            return Ok(self.close_with(DisconnectReason::PeerClosed));
        }
        if progress.plaintext.is_empty() {
            return Ok(Vec::new());
        }
        let Some(cleartext) = self.tls_cleartext.push(
            frame.channel_id,
            frame.flags,
            frame.announced_total,
            &progress.plaintext,
        )?
        else {
            return Ok(Vec::new());
        };
        let (message_id, payload) = split_message(&cleartext)?;
        self.handle_message(frame.channel_id, message_id, payload, now)
    }

    fn handle_message(
        &mut self,
        channel_id: u8,
        message_id: u16,
        payload: &[u8],
        now: Instant,
    ) -> Result<Vec<Event>> {
        self.trace.last_message = Some((channel_id, message_id));
        let assembly = self.receive_started.take();
        if channel_id == channel::VIDEO
            && matches!(message_id, message::MEDIA | message::MEDIA_WITH_TIMESTAMP)
        {
            // A frame with no recorded start arrived whole in a read that
            // also completed an earlier message.
            let (assembly, reads) = assembly.map_or((Duration::ZERO, 1), |(started, reads)| {
                (started.elapsed(), reads)
            });
            self.link.record_video(assembly, reads);
        }
        if channel_id == channel::CONTROL {
            return self.handle_control(message_id, payload, now);
        }
        if message_id == message::CHANNEL_OPEN_REQUEST {
            if !self.advertises(channel_id) {
                return Err(Error::InvalidMessage(
                    "phone opened an unadvertised channel",
                ));
            }
            self.trace.opened_channels.push(channel_id);
            self.send_encrypted(
                channel_id,
                FLAG_ENCRYPTED_CONTROL,
                message::CHANNEL_OPEN_RESPONSE,
                &status_ok(),
            )?;
            return Ok(Vec::new());
        }
        match channel_id {
            channel::SENSOR => self.handle_sensor(message_id, payload),
            channel::VIDEO => self.handle_video(message_id, payload),
            channel::MEDIA_AUDIO | channel::GUIDANCE_AUDIO | channel::SYSTEM_AUDIO => {
                self.handle_audio(channel_id, message_id, payload)
            }
            channel::INPUT => self.handle_input(message_id),
            channel::MICROPHONE => self.handle_microphone(message_id, payload),
            channel::BLUETOOTH if self.config.bluetooth_address.is_some() => {
                self.handle_bluetooth(message_id, payload)
            }
            _ => Err(Error::InvalidMessage(
                "message is on an unadvertised channel",
            )),
        }
    }

    fn advertises(&self, channel_id: u8) -> bool {
        channel::advertised(
            channel_id,
            self.config.audio_output,
            self.config.bluetooth_address.is_some(),
        )
    }

    fn handle_control(
        &mut self,
        message_id: u16,
        payload: &[u8],
        now: Instant,
    ) -> Result<Vec<Event>> {
        match message_id {
            message::SERVICE_DISCOVERY_REQUEST => {
                if self.state != SessionState::ServiceDiscovery {
                    return Err(Error::InvalidMessage(
                        "service discovery request is out of order",
                    ));
                }
                let request: ServiceDiscoveryRequest =
                    decode("service discovery request", payload)?;
                validate_remote_identity_lengths(&request)?;
                self.send_encrypted(
                    channel::CONTROL,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::SERVICE_DISCOVERY_RESPONSE,
                    &service_discovery_payload_with_density(
                        &self.config.head_unit_name,
                        self.config.video_density_dpi,
                        self.config.audio_output,
                        self.config.bluetooth_address.as_deref(),
                    )?,
                )?;
                self.state = SessionState::ChannelSetup;
                self.send_ping(now)?;
                Ok(Vec::new())
            }
            message::CHANNEL_OPEN_REQUEST => {
                let request: ChannelOpenRequest = decode("channel open request", payload)?;
                let service_id = request.service_id.ok_or(Error::InvalidMessage(
                    "channel open request omits its service ID",
                ))?;
                let _priority = request.priority.ok_or(Error::InvalidMessage(
                    "channel open request omits its priority",
                ))?;
                let channel_id = u8::try_from(service_id)
                    .map_err(|_| Error::InvalidMessage("channel ID is outside the wire range"))?;
                if !self.advertises(channel_id) {
                    return Err(Error::InvalidMessage(
                        "phone opened an unadvertised channel",
                    ));
                }
                self.trace.opened_channels.push(channel_id);
                self.send_encrypted(
                    channel::CONTROL,
                    FLAG_ENCRYPTED_CONTROL,
                    message::CHANNEL_OPEN_RESPONSE,
                    &status_ok(),
                )?;
                Ok(Vec::new())
            }
            message::CHANNEL_OPEN_RESPONSE => {
                let response: crate::proto::StatusResponse =
                    decode("channel open response", payload)?;
                if response.status != STATUS_OK {
                    return Err(Error::InvalidMessage(
                        "phone rejected a channel open response",
                    ));
                }
                Ok(Vec::new())
            }
            message::PING_REQUEST => {
                let ping: Ping = decode("ping request", payload)?;
                let timestamp = ping
                    .timestamp
                    .ok_or(Error::InvalidMessage("ping request omits its timestamp"))?;
                let arrived = Instant::now();
                let gap = self.phone_ping.map(|(previous, previous_arrival)| {
                    (
                        arrived.saturating_duration_since(previous_arrival),
                        timestamp.wrapping_sub(previous),
                    )
                });
                self.link.record_phone_ping(gap);
                self.phone_ping = Some((timestamp, arrived));
                self.write_plain(
                    channel::CONTROL,
                    message::PING_RESPONSE,
                    &Ping {
                        timestamp: Some(timestamp),
                    }
                    .encode_to_vec(),
                )?;
                Ok(Vec::new())
            }
            message::PING_RESPONSE => {
                let ping: Ping = decode("ping response", payload)?;
                if ping.timestamp.is_none() {
                    return Err(Error::InvalidMessage("ping response omits its timestamp"));
                }
                if let Some((timestamp, sent_at)) = self.ping_sent
                    && ping.timestamp == Some(timestamp)
                {
                    self.link.record_ping(sent_at.elapsed());
                    self.ping_sent = None;
                }
                self.last_inbound = Some(now);
                Ok(Vec::new())
            }
            message::AUDIO_FOCUS_REQUEST => {
                let request: AudioFocusRequest = decode("audio focus request", payload)?;
                let focus_type = request.audio_focus_type.ok_or(Error::InvalidMessage(
                    "audio focus request omits its focus type",
                ))?;
                let focus_state = audio_focus_state(focus_type);
                self.send_encrypted(
                    channel::CONTROL,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::AUDIO_FOCUS_RESPONSE,
                    &[0x08, focus_state],
                )?;
                Ok(Vec::new())
            }
            message::VOICE_SESSION_NOTIFICATION => {
                let notification: VoiceSessionNotification =
                    decode("voice session notification", payload)?;
                let active = match notification.status {
                    Some(1) => true,
                    Some(2) => false,
                    _ => {
                        return Err(Error::InvalidMessage(
                            "voice session notification has an invalid status",
                        ));
                    }
                };
                Ok(vec![Event::VoiceSession { active }])
            }
            message::BATTERY_STATUS_NOTIFICATION => {
                let notification: BatteryStatusNotification =
                    decode("battery status notification", payload)?;
                let level = notification.battery_level.ok_or(Error::InvalidMessage(
                    "battery status notification omits its level",
                ))?;
                if level > 100 {
                    return Err(Error::InvalidMessage(
                        "battery status notification level exceeds 100",
                    ));
                }
                Ok(vec![Event::Battery {
                    level,
                    critical: notification.critical.unwrap_or(false),
                    time_remaining_seconds: notification.time_remaining_seconds,
                }])
            }
            message::NAVIGATION_FOCUS_REQUEST => {
                self.send_encrypted(
                    channel::CONTROL,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::NAVIGATION_FOCUS_RESPONSE,
                    payload,
                )?;
                Ok(Vec::new())
            }
            message::BINDING_REQUEST => {
                self.send_encrypted(
                    channel::CONTROL,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::BINDING_RESPONSE,
                    &status_ok(),
                )?;
                Ok(Vec::new())
            }
            message::SHUTDOWN_REQUEST => {
                let request: ShutdownRequest = decode("shutdown request", payload)?;
                if request.reason.is_none() {
                    return Err(Error::InvalidMessage("shutdown request omits its reason"));
                }
                self.send_encrypted(
                    channel::CONTROL,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::SHUTDOWN_RESPONSE,
                    &[],
                )?;
                Ok(self.close_with(DisconnectReason::PeerShutdown))
            }
            message::SHUTDOWN_RESPONSE if self.state == SessionState::Closing => {
                if !payload.is_empty() {
                    return Err(Error::InvalidMessage("shutdown response is not empty"));
                }
                Ok(self.close_with(DisconnectReason::LocalShutdown))
            }
            message::CHANNEL_CLOSE => {
                if !payload.is_empty() {
                    return Err(Error::InvalidMessage(
                        "channel close notification is not empty",
                    ));
                }
                Ok(Vec::new())
            }
            message::AV_SETUP_REQUEST => {
                let setup: Setup = decode("defensive control-channel AV setup", payload)?;
                if !matches!(
                    setup.media_codec_type,
                    Some(MEDIA_CODEC_PCM | MEDIA_CODEC_H264_BASELINE)
                ) {
                    return Err(Error::InvalidMessage(
                        "control-channel AV setup selects an unsupported codec",
                    ));
                }
                Ok(Vec::new())
            }
            _ => Err(Error::InvalidMessage("unsupported control message")),
        }
    }

    fn handle_sensor(&mut self, message_id: u16, payload: &[u8]) -> Result<Vec<Event>> {
        if message_id != message::SENSOR_START_REQUEST {
            return Err(Error::InvalidMessage("unsupported sensor message"));
        }
        let request: SensorRequest = decode("sensor start request", payload)?;
        let sensor_type = request.sensor_type.ok_or(Error::InvalidMessage(
            "sensor start request omits its sensor type",
        ))?;
        if !matches!(sensor_type, 10 | 13) {
            return Err(Error::InvalidMessage(
                "phone requested an unadvertised sensor",
            ));
        }
        self.send_encrypted(
            channel::SENSOR,
            FLAG_ENCRYPTED_SIGNAL,
            message::SENSOR_START_RESPONSE,
            &status_ok(),
        )?;
        let initial = match sensor_type {
            13 => [0x6a, 0x02, 0x08, 0x00],
            10 => [0x52, 0x02, 0x08, u8::from(self.config.initial_night_mode)],
            _ => unreachable!("sensor type was validated"),
        };
        if sensor_type == 10 {
            self.night_mode = Some(self.config.initial_night_mode);
        }
        self.send_encrypted(
            channel::SENSOR,
            FLAG_ENCRYPTED_SIGNAL,
            message::SENSOR_BATCH,
            &initial,
        )?;
        Ok(Vec::new())
    }

    fn handle_video(&mut self, message_id: u16, payload: &[u8]) -> Result<Vec<Event>> {
        match message_id {
            message::AV_SETUP_REQUEST => {
                let setup: Setup = decode("video setup request", payload)?;
                if setup.media_codec_type != Some(MEDIA_CODEC_H264_BASELINE) {
                    return Err(Error::InvalidMessage(
                        "phone selected an unadvertised video codec",
                    ));
                }
                self.send_encrypted(
                    channel::VIDEO,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::AV_SETUP_RESPONSE,
                    &setup_ok_with_window(VIDEO_MAX_UNACKED),
                )?;
                self.send_encrypted(
                    channel::VIDEO,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::VIDEO_FOCUS_INDICATION,
                    &[0x08, 0x01],
                )?;
                let first_connection = self.state != SessionState::Running;
                self.state = SessionState::Running;
                Ok(if first_connection {
                    vec![Event::Connected]
                } else {
                    Vec::new()
                })
            }
            message::AV_START => {
                self.record_start(channel::VIDEO, payload)?;
                Ok(Vec::new())
            }
            message::MEDIA | message::MEDIA_WITH_TIMESTAMP => {
                let (timestamp, annex_b) = media_payload(message_id, payload)?;
                if !is_annex_b(annex_b) {
                    return Err(Error::InvalidMessage("video frame is not Annex-B H.264"));
                }
                self.send_media_ack(channel::VIDEO)?;
                Ok(vec![Event::Video {
                    timestamp,
                    annex_b: annex_b.to_vec(),
                }])
            }
            message::VIDEO_FOCUS_REQUEST => {
                self.send_encrypted(
                    channel::VIDEO,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::VIDEO_FOCUS_INDICATION,
                    &[0x08, 0x01],
                )?;
                Ok(Vec::new())
            }
            message::VIDEO_FOCUS_INDICATION => {
                let notification: VideoFocusNotification =
                    decode("video focus indication", payload)?;
                if !matches!(notification.focus, Some(1..=3)) {
                    return Err(Error::InvalidMessage(
                        "video focus indication has an invalid focus state",
                    ));
                }
                Ok(Vec::new())
            }
            message::AV_STOP => {
                self.sessions.remove(&channel::VIDEO);
                Ok(Vec::new())
            }
            _ => Err(Error::InvalidMessage("unsupported video message")),
        }
    }

    fn handle_audio(
        &mut self,
        channel_id: u8,
        message_id: u16,
        payload: &[u8],
    ) -> Result<Vec<Event>> {
        match message_id {
            message::AV_SETUP_REQUEST => {
                let setup: Setup = decode("audio setup request", payload)?;
                if setup.media_codec_type != Some(MEDIA_CODEC_PCM) {
                    return Err(Error::InvalidMessage(
                        "phone selected an unadvertised audio codec",
                    ));
                }
                self.send_encrypted(
                    channel_id,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::AV_SETUP_RESPONSE,
                    &setup_ok_with_window(AUDIO_MAX_UNACKED),
                )?;
                Ok(Vec::new())
            }
            message::AV_START => {
                self.record_start(channel_id, payload)?;
                Ok(Vec::new())
            }
            message::MEDIA | message::MEDIA_WITH_TIMESTAMP => {
                let (timestamp, pcm) = media_payload(message_id, payload)?;
                let audio_channel = audio_channel(channel_id)?;
                let frame_size = if audio_channel == AudioChannel::Media {
                    4
                } else {
                    2
                };
                if pcm.is_empty() || pcm.len() % frame_size != 0 {
                    return Err(Error::InvalidMessage(
                        "PCM payload is not sample-frame aligned",
                    ));
                }
                self.send_media_ack(channel_id)?;
                Ok(vec![Event::Audio {
                    channel: audio_channel,
                    timestamp,
                    pcm_s16le: pcm.to_vec(),
                }])
            }
            message::AV_STOP => {
                self.sessions.remove(&channel_id);
                Ok(Vec::new())
            }
            _ => Err(Error::InvalidMessage("unsupported audio message")),
        }
    }

    fn handle_input(&mut self, message_id: u16) -> Result<Vec<Event>> {
        if message_id != message::KEY_BINDING_REQUEST {
            return Err(Error::InvalidMessage("unsupported input channel message"));
        }
        self.send_encrypted(
            channel::INPUT,
            FLAG_ENCRYPTED_SIGNAL,
            message::KEY_BINDING_RESPONSE,
            &status_ok(),
        )?;
        Ok(Vec::new())
    }

    fn handle_bluetooth(&mut self, message_id: u16, payload: &[u8]) -> Result<Vec<Event>> {
        if message_id == message::BLUETOOTH_AUTHENTICATION_RESULT {
            let result: BluetoothAuthenticationResult =
                decode("Bluetooth authentication result", payload)?;
            return Ok(vec![Event::BluetoothAuthenticated {
                status: result.status,
            }]);
        }
        if message_id != message::BLUETOOTH_PAIRING_REQUEST {
            return Ok(Vec::new());
        }
        let request: BluetoothPairingRequest = decode("Bluetooth pairing request", payload)?;
        let phone_address = request
            .phone_address
            .filter(|address| address.len() <= MAX_PHONE_ADDRESS_BYTES);
        self.bluetooth_pairing_pending = true;
        Ok(vec![Event::BluetoothPairing { phone_address }])
    }

    fn handle_microphone(&mut self, message_id: u16, payload: &[u8]) -> Result<Vec<Event>> {
        match message_id {
            message::AV_SETUP_REQUEST => {
                let setup: Setup = decode("microphone setup request", payload)?;
                if setup.media_codec_type != Some(MEDIA_CODEC_PCM) {
                    return Err(Error::InvalidMessage(
                        "phone selected an unadvertised microphone codec",
                    ));
                }
                self.send_encrypted(
                    channel::MICROPHONE,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::AV_SETUP_RESPONSE,
                    &setup_ok(),
                )?;
                Ok(Vec::new())
            }
            message::MICROPHONE_OPEN_REQUEST => {
                let request: MicrophoneRequest = decode("microphone open request", payload)?;
                let open = request.open.ok_or(Error::InvalidMessage(
                    "microphone request omits its open state",
                ))?;
                let window = request.max_unacked.unwrap_or(1);
                if !(1..=i32::try_from(MAX_MIC_WINDOW).unwrap_or(i32::MAX)).contains(&window) {
                    return Err(Error::InvalidMessage(
                        "microphone flow-control window is invalid",
                    ));
                }
                self.microphone_max_unacked = usize::try_from(window)
                    .map_err(|_| Error::InvalidMessage("microphone window is negative"))?;
                self.send_encrypted(
                    channel::MICROPHONE,
                    FLAG_ENCRYPTED_SIGNAL,
                    message::MICROPHONE_OPEN_RESPONSE,
                    &MicrophoneResponse {
                        status: STATUS_OK,
                        session_id: Some(MICROPHONE_SESSION_ID),
                    }
                    .encode_to_vec(),
                )?;
                let changed = open != self.microphone_open;
                self.microphone_open = open;
                self.microphone_unacked = 0;
                self.microphone_queue.clear();
                Ok(if changed || open {
                    vec![Event::Microphone {
                        open,
                        sample_rate: 16_000,
                        channels: 1,
                    }]
                } else {
                    Vec::new()
                })
            }
            message::AV_ACK => {
                let acknowledgment: Ack = decode("microphone ACK", payload)?;
                if acknowledgment
                    .session_id
                    .is_some_and(|session_id| !matches!(session_id, 0 | MICROPHONE_SESSION_ID))
                {
                    return Err(Error::InvalidMessage(
                        "microphone ACK has the wrong session ID",
                    ));
                }
                let acknowledgment_count = acknowledgment.ack.unwrap_or(1);
                if !(1..=u32::try_from(MAX_MIC_WINDOW).unwrap_or(u32::MAX))
                    .contains(&acknowledgment_count)
                {
                    return Err(Error::InvalidMessage(
                        "microphone ACK has an invalid acknowledgment count",
                    ));
                }
                if self.microphone_unacked == 0 {
                    return Ok(Vec::new());
                }
                let acknowledgment_count = usize::try_from(acknowledgment_count)
                    .map_err(|_| Error::InvalidMessage("microphone ACK count overflows usize"))?;
                self.microphone_unacked =
                    self.microphone_unacked.saturating_sub(acknowledgment_count);
                self.drain_microphone_queue()?;
                Ok(Vec::new())
            }
            message::AV_STOP => {
                let changed = self.microphone_open;
                self.microphone_open = false;
                self.microphone_unacked = 0;
                self.microphone_queue.clear();
                Ok(if changed {
                    vec![Event::Microphone {
                        open: false,
                        sample_rate: 16_000,
                        channels: 1,
                    }]
                } else {
                    Vec::new()
                })
            }
            _ => Err(Error::InvalidMessage("unsupported microphone message")),
        }
    }

    fn record_start(&mut self, channel_id: u8, payload: &[u8]) -> Result<()> {
        let start: Start = decode("AV start", payload)?;
        let session_id = start
            .session_id
            .ok_or(Error::InvalidMessage("AV start omits its session ID"))?;
        let configuration_index = start.configuration_index.ok_or(Error::InvalidMessage(
            "AV start omits its configuration index",
        ))?;
        if session_id < 0 || configuration_index > 1 {
            return Err(Error::InvalidMessage(
                "AV start selected an invalid session or config",
            ));
        }
        self.sessions.insert(channel_id, session_id);
        Ok(())
    }

    fn send_media_ack(&mut self, channel_id: u8) -> Result<()> {
        let session_id = self
            .sessions
            .get(&channel_id)
            .copied()
            .ok_or(Error::InvalidMessage(
                "media arrived before its AV start indication",
            ))?;
        self.send_encrypted(
            channel_id,
            FLAG_ENCRYPTED_SIGNAL,
            message::AV_ACK,
            &ack(session_id),
        )
    }

    fn send_touch(
        &mut self,
        timestamp_micros: u64,
        action: TouchAction,
        action_index: u32,
        pointers: &[TouchPointer],
    ) -> Result<()> {
        if pointers.is_empty() || pointers.len() > MAX_TOUCH_POINTERS {
            return Err(Error::InvalidMessage("touch pointer count is invalid"));
        }
        let action_index_usize = usize::try_from(action_index)
            .map_err(|_| Error::InvalidMessage("touch action index is invalid"))?;
        if action_index_usize >= pointers.len() {
            return Err(Error::InvalidMessage("touch action index is out of range"));
        }
        let mut ids = BTreeSet::new();
        for pointer in pointers {
            if pointer.x >= VIDEO_WIDTH || pointer.y >= VIDEO_HEIGHT || !ids.insert(pointer.id) {
                return Err(Error::InvalidMessage(
                    "touch coordinates or pointer identifiers are invalid",
                ));
            }
        }
        let payload = input_touch(
            timestamp_micros,
            action as i32,
            action_index,
            pointers.iter().map(|pointer| TouchPointMessage {
                x: pointer.x,
                y: pointer.y,
                pointer_id: pointer.id,
            }),
        );
        self.send_encrypted(
            channel::INPUT,
            FLAG_ENCRYPTED_SIGNAL,
            message::INPUT_REPORT,
            &payload,
        )
    }

    fn queue_microphone(&mut self, timestamp: u64, pcm: Vec<u8>) -> Result<()> {
        if !self.microphone_open {
            return Ok(());
        }
        if pcm.is_empty() || pcm.len() > MAX_MIC_PCM || pcm.len() % 2 != 0 {
            return Err(Error::InvalidMessage(
                "microphone PCM is empty, oversized, or sample-misaligned",
            ));
        }
        if self.microphone_unacked < self.microphone_max_unacked {
            return self.send_microphone_frame(timestamp, &pcm);
        }
        if self.microphone_queue.len() >= MAX_MIC_BACKLOG {
            return Err(Error::Backpressure);
        }
        self.microphone_queue
            .push_back(QueuedMicrophoneFrame { timestamp, pcm });
        Ok(())
    }

    fn drain_microphone_queue(&mut self) -> Result<()> {
        while self.microphone_unacked < self.microphone_max_unacked {
            let Some(frame) = self.microphone_queue.pop_front() else {
                break;
            };
            self.send_microphone_frame(frame.timestamp, &frame.pcm)?;
        }
        Ok(())
    }

    fn send_microphone_frame(&mut self, timestamp: u64, pcm: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(8 + pcm.len());
        payload.extend_from_slice(&timestamp.to_be_bytes());
        payload.extend_from_slice(pcm);
        self.send_encrypted(
            channel::MICROPHONE,
            FLAG_ENCRYPTED_SIGNAL,
            message::MEDIA_WITH_TIMESTAMP,
            &payload,
        )?;
        self.microphone_unacked = self
            .microphone_unacked
            .checked_add(1)
            .ok_or(Error::InvalidMessage("microphone unacked count overflow"))?;
        Ok(())
    }

    fn send_ping(&mut self, now: Instant) -> Result<()> {
        let started = self.started_at.ok_or(Error::InvalidState {
            operation: "sending a ping before session start",
        })?;
        let micros = now.saturating_duration_since(started).as_micros();
        let timestamp = i64::try_from(micros).unwrap_or(i64::MAX);
        self.write_plain(
            channel::CONTROL,
            message::PING_REQUEST,
            &Ping {
                timestamp: Some(timestamp),
            }
            .encode_to_vec(),
        )?;
        self.last_ping = Some(now);
        self.ping_sent = Some((timestamp, Instant::now()));
        Ok(())
    }

    fn send_encrypted(
        &mut self,
        channel_id: u8,
        flags: u8,
        message_id: u16,
        payload: &[u8],
    ) -> Result<()> {
        let mut cleartext = Vec::with_capacity(2 + payload.len());
        cleartext.extend_from_slice(&message_id.to_be_bytes());
        cleartext.extend_from_slice(payload);
        let records = self
            .tls
            .as_mut()
            .ok_or(Error::Tls("encrypted write has no TLS state"))?
            .encrypt(&cleartext)?;
        self.write_tls_records(channel_id, flags, cleartext.len(), &records)
    }

    fn write_tls_records(
        &mut self,
        channel_id: u8,
        flags: u8,
        cleartext_len: usize,
        records: &[Vec<u8>],
    ) -> Result<()> {
        if records.is_empty() {
            return Err(Error::Tls("TLS produced no encrypted record"));
        }
        let base_flags = flags & !FRAME_TYPE_MASK;
        if !matches!(
            base_flags,
            FLAG_ENCRYPTED_SPECIFIC_BASE | FLAG_ENCRYPTED_CONTROL_BASE
        ) {
            return Err(Error::InvalidFrame("outbound encrypted flags are invalid"));
        }
        if records.len() == 1 {
            let frame = encode_encrypted_frame(
                channel_id,
                base_flags | FRAME_TYPE_BULK,
                &records[0],
                None,
            )?;
            return self.write_raw(&frame);
        }
        if cleartext_len == 0 || cleartext_len > MAX_REASSEMBLED_PAYLOAD {
            return Err(Error::InvalidFrame(
                "fragmented TLS write has an invalid cleartext total",
            ));
        }
        for (index, record) in records.iter().enumerate() {
            let (fragment_flags, announced_total) = if index == 0 {
                (base_flags | FRAME_TYPE_FIRST, Some(cleartext_len))
            } else if index + 1 == records.len() {
                (base_flags | FRAME_TYPE_LAST, None)
            } else {
                (base_flags | FRAME_TYPE_MIDDLE, None)
            };
            let frame =
                encode_encrypted_frame(channel_id, fragment_flags, record, announced_total)?;
            self.write_raw(&frame)?;
        }
        Ok(())
    }

    fn write_plain(&mut self, channel_id: u8, message_id: u16, payload: &[u8]) -> Result<()> {
        let frame = encode_plain_frame(channel_id, FLAG_PLAINTEXT, message_id, payload)?;
        self.write_raw(&frame)
    }

    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.transport.write_all(bytes)?;
        self.transport.flush()?;
        Ok(())
    }

    fn close_with(&mut self, reason: DisconnectReason) -> Vec<Event> {
        if self.state != SessionState::Closed {
            self.trace.closed_in = Some(self.state);
        }
        self.state = SessionState::Closed;
        self.microphone_open = false;
        self.microphone_unacked = 0;
        self.microphone_queue.clear();
        self.keyframe_deadline = None;
        if self.disconnected_emitted {
            Vec::new()
        } else {
            self.disconnected_emitted = true;
            vec![Event::Disconnected { reason }]
        }
    }
}

fn split_message(message: &[u8]) -> Result<(u16, &[u8])> {
    if message.len() < 2 {
        return Err(Error::InvalidMessage("message has no two-byte ID"));
    }
    Ok((u16::from_be_bytes([message[0], message[1]]), &message[2..]))
}

fn validate_remote_identity_lengths(request: &ServiceDiscoveryRequest) -> Result<()> {
    for value in [
        request.device_name.as_deref(),
        request.device_brand.as_deref(),
        request
            .phone_info
            .as_ref()
            .and_then(|phone| phone.instance_id.as_deref()),
        request
            .phone_info
            .as_ref()
            .and_then(|phone| phone.connectivity_lifetime_id.as_deref()),
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > 256 || value.chars().any(char::is_control) {
            return Err(Error::InvalidMessage("remote identity field is invalid"));
        }
    }
    Ok(())
}

fn media_payload(message_id: u16, payload: &[u8]) -> Result<(Option<u64>, &[u8])> {
    if message_id == message::MEDIA_WITH_TIMESTAMP {
        if payload.len() < 8 {
            return Err(Error::InvalidMessage(
                "timestamped media is shorter than its timestamp",
            ));
        }
        let timestamp = u64::from_be_bytes([
            payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
            payload[7],
        ]);
        Ok((Some(timestamp), &payload[8..]))
    } else {
        Ok((None, payload))
    }
}

fn is_annex_b(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0, 0, 1]) || bytes.starts_with(&[0, 0, 0, 1])
}

fn audio_channel(channel_id: u8) -> Result<AudioChannel> {
    match channel_id {
        channel::MEDIA_AUDIO => Ok(AudioChannel::Media),
        channel::GUIDANCE_AUDIO => Ok(AudioChannel::Guidance),
        channel::SYSTEM_AUDIO => Ok(AudioChannel::System),
        _ => Err(Error::InvalidMessage("channel is not an audio sink")),
    }
}

const fn audio_focus_state(request: i32) -> u8 {
    match request {
        1 => 1,
        2 | 3 => 2,
        4 => 3,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use rustls::{
        ServerConfig, ServerConnection,
        server::{ClientHello, ResolvesServerCert},
        sign::CertifiedKey,
    };

    use super::*;

    #[test]
    fn link_window_keeps_each_ping_until_its_samples_run_out() {
        let mut window = LinkWindow::default();
        window.record_phone_ping(None);
        assert_eq!((window.phone_pings, window.phone_ping_gaps), (1, 0));
        for ping in 0..10_u32 {
            window.record_phone_ping(Some((
                Duration::from_millis(1_000 + u64::from(ping)),
                1_000_000 + i64::from(ping),
            )));
            window.record_ping(Duration::from_millis(u64::from(ping) + 1));
        }
        assert_eq!(window.phone_pings, 11);
        assert_eq!(window.phone_ping_gaps, LINK_SAMPLES as u64);
        assert_eq!(window.phone_ping_arrival_gap_micros[0], 1_000_000);
        assert_eq!(window.phone_ping_arrival_gap_micros[7], 1_007_000);
        assert_eq!(window.phone_ping_timestamp_gap[7], 1_000_007);
        assert_eq!(window.ping_round_trips, 10);
        assert_eq!(window.ping_round_trip_micros[0], 1_000);
        assert_eq!(window.ping_round_trip_micros[7], 8_000);
        assert_eq!(window.ping_maximum_micros, 10_000);
    }

    #[derive(Debug, Default)]
    struct MemoryTransport {
        written: Vec<u8>,
    }

    impl Read for MemoryTransport {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::TimedOut, "test timeout"))
        }
    }

    impl Write for MemoryTransport {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl MemoryTransport {
        fn take_written(&mut self) -> Vec<u8> {
            std::mem::take(&mut self.written)
        }
    }

    #[derive(Debug)]
    struct FixedServerIdentity(Arc<CertifiedKey>);

    impl ResolvesServerCert for FixedServerIdentity {
        fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(Arc::clone(&self.0))
        }
    }

    #[derive(Debug)]
    struct PhoneTls {
        connection: ServerConnection,
    }

    impl PhoneTls {
        fn new() -> Self {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut cert_reader =
                std::io::BufReader::new(include_bytes!("../certs/hu-cert.pem").as_slice());
            let certificates = rustls_pemfile::certs(&mut cert_reader)
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("server certificates");
            let mut key_reader =
                std::io::BufReader::new(include_bytes!("../certs/hu-key.pem").as_slice());
            let key = rustls_pemfile::private_key(&mut key_reader)
                .expect("server key parse")
                .expect("server key");
            let signing_key = provider
                .key_provider
                .load_private_key(key)
                .expect("server signing key");
            let identity = Arc::new(CertifiedKey::new(certificates, signing_key));
            let config = ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS12])
                .expect("TLS 1.2")
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(FixedServerIdentity(identity)));
            Self {
                connection: ServerConnection::new(Arc::new(config)).expect("server"),
            }
        }

        fn accept_tls(&mut self, bytes: &[u8]) -> Vec<u8> {
            let mut cursor = Cursor::new(bytes);
            while usize::try_from(cursor.position()).expect("cursor position") < bytes.len() {
                let read = self
                    .connection
                    .read_tls(&mut cursor)
                    .expect("phone TLS read");
                assert_ne!(read, 0, "phone TLS reader must make progress");
            }
            self.connection
                .process_new_packets()
                .expect("phone TLS packets");
            let mut plaintext = Vec::new();
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                match self.connection.reader().read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => plaintext.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("phone plaintext read: {error}"),
                }
            }
            plaintext
        }

        fn take_tls(&mut self) -> Vec<u8> {
            let mut bytes = Vec::new();
            while self.connection.wants_write() {
                self.connection
                    .write_tls(&mut bytes)
                    .expect("phone TLS write");
            }
            bytes
        }

        fn encrypted_frames(&mut self, channel_id: u8, message_id: u16, payload: &[u8]) -> Vec<u8> {
            self.encrypted_frame_list(channel_id, message_id, payload)
                .concat()
        }

        fn encrypted_frame_list(
            &mut self,
            channel_id: u8,
            message_id: u16,
            payload: &[u8],
        ) -> Vec<Vec<u8>> {
            let mut cleartext = Vec::with_capacity(2 + payload.len());
            cleartext.extend_from_slice(&message_id.to_be_bytes());
            cleartext.extend_from_slice(payload);
            self.connection
                .writer()
                .write_all(&cleartext)
                .expect("phone application write");
            let records = split_tls_records(&self.take_tls()).expect("phone TLS records");
            records
                .iter()
                .enumerate()
                .map(|(index, record)| {
                    let (flags, announced_total) = if records.len() == 1 {
                        (FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_BULK, None)
                    } else if index == 0 {
                        (
                            FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_FIRST,
                            Some(cleartext.len()),
                        )
                    } else if index + 1 == records.len() {
                        (FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_LAST, None)
                    } else {
                        (FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_MIDDLE, None)
                    };
                    encode_encrypted_frame(channel_id, flags, record, announced_total)
                        .expect("encrypted frame")
                })
                .collect()
        }
    }

    fn decode_written(bytes: &[u8]) -> Vec<PhysicalFrame> {
        FrameDecoder::new().push(bytes).expect("written frames")
    }

    fn secure_session() -> (Session<MemoryTransport>, PhoneTls, Instant) {
        let now = Instant::now();
        let mut session = session();
        let mut phone = PhoneTls::new();
        session.start_at(now).expect("start");
        session.transport.take_written();
        let version = encode_plain_frame(
            channel::CONTROL,
            FLAG_PLAINTEXT,
            message::VERSION_RESPONSE,
            &[0, 1, 0, 7, 0, 0],
        )
        .expect("version");
        session.ingest_at(&version, now).expect("version response");

        for _ in 0..8 {
            let hu_frames = decode_written(&session.transport.take_written());
            for frame in hu_frames {
                if frame.flags & FLAG_ENCRYPTED != 0 {
                    let _ = phone.accept_tls(&frame.payload);
                    continue;
                }
                let (message_id, payload) = split_message(&frame.payload).expect("plain message");
                if message_id == message::TLS_HANDSHAKE {
                    let _ = phone.accept_tls(payload);
                }
            }
            let phone_tls = phone.take_tls();
            if !phone_tls.is_empty() {
                let handshake = encode_plain_frame(
                    channel::CONTROL,
                    FLAG_PLAINTEXT,
                    message::TLS_HANDSHAKE,
                    &phone_tls,
                )
                .expect("handshake frame");
                session
                    .ingest_at(&handshake, now)
                    .expect("handshake response");
            }
            if session.state() == SessionState::ServiceDiscovery
                && !phone.connection.is_handshaking()
            {
                break;
            }
        }
        assert_eq!(session.state(), SessionState::ServiceDiscovery);
        session.transport.take_written();
        (session, phone, now)
    }

    fn discovered_session() -> (Session<MemoryTransport>, PhoneTls, Instant) {
        let (mut session, mut phone, now) = secure_session();
        let request = phone.encrypted_frames(
            channel::CONTROL,
            message::SERVICE_DISCOVERY_REQUEST,
            &ServiceDiscoveryRequest {
                device_name: None,
                device_brand: None,
                phone_info: None,
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&request, now + Duration::from_millis(1))
            .expect("service discovery");
        assert_eq!(session.state(), SessionState::ChannelSetup);
        (session, phone, now)
    }

    fn drain_hu_messages(
        session: &mut Session<MemoryTransport>,
        phone: &mut PhoneTls,
    ) -> Vec<(u8, u16, Vec<u8>)> {
        let frames = decode_written(&session.transport.take_written());
        let mut messages = Vec::new();
        for frame in frames {
            let cleartext = if frame.flags & FLAG_ENCRYPTED != 0 {
                phone.accept_tls(&frame.payload)
            } else {
                frame.payload
            };
            if cleartext.is_empty() {
                continue;
            }
            let (message_id, payload) = split_message(&cleartext).expect("HU message");
            messages.push((frame.channel_id, message_id, payload.to_vec()));
        }
        messages
    }

    fn session() -> Session<MemoryTransport> {
        Session::new(MemoryTransport::default(), SessionConfig::default()).expect("session")
    }

    #[test]
    fn version_request_is_the_livi_1_7_golden() {
        let mut session = session();
        session.start_at(Instant::now()).expect("start");
        assert_eq!(session.transport.written, [0, 3, 0, 6, 0, 1, 0, 1, 0, 7]);
        assert_eq!(session.state(), SessionState::Version);
    }

    #[test]
    fn version_response_requires_exact_1_7_and_starts_tls() {
        let now = Instant::now();
        let mut session = session();
        session.start_at(now).expect("start");
        session.transport.written.clear();
        let response = encode_plain_frame(
            channel::CONTROL,
            FLAG_PLAINTEXT,
            message::VERSION_RESPONSE,
            &[0, 1, 0, 7, 0, 0],
        )
        .expect("response");
        session.ingest_at(&response, now).expect("version");
        assert_eq!(session.state(), SessionState::TlsHandshake);
        let frames = FrameDecoder::new()
            .push(&session.transport.written)
            .expect("ClientHello frame");
        assert_eq!(frames.len(), 1);
        assert_eq!(
            &frames[0].payload[..2],
            &message::TLS_HANDSHAKE.to_be_bytes()
        );
        assert_eq!(frames[0].payload[2], 0x16);
    }

    #[test]
    fn version_response_with_trailing_bytes_starts_tls() {
        let mut session = session();
        session.start_at(Instant::now()).expect("start");
        session
            .handle_version_response(&[0, 1, 0, 7, 0, 0, 0, 1, 0, 6])
            .expect("version");
        assert_eq!(session.state(), SessionState::TlsHandshake);
    }

    #[test]
    fn mismatched_or_truncated_version_fails_closed() {
        let mut session = session();
        let now = Instant::now();
        session.start_at(now).expect("start");
        assert!(matches!(
            session.handle_version_response(&[0, 1]),
            Err(Error::VersionResponseTruncated { actual: 2 })
        ));
        assert!(
            session
                .handle_version_response(&[0, 1, 0, 6, 0, 0])
                .is_err()
        );
    }

    #[test]
    fn read_timeout_drives_the_startup_watchdog() {
        let now = Instant::now();
        let mut session = session();
        session.start_at(now).expect("start");
        let events = session
            .poll_at(now + Duration::from_secs(31))
            .expect("poll timeout");
        assert_eq!(
            events,
            [Event::Disconnected {
                reason: DisconnectReason::StartupTimeout
            }]
        );
        assert!(!session.is_alive());
    }

    #[test]
    fn cancellation_before_tls_is_bounded_and_idempotent() {
        let now = Instant::now();
        let mut session = session();
        session.start_at(now).expect("start");
        assert_eq!(
            session.cancel_at(now).expect("cancel"),
            [Event::Disconnected {
                reason: DisconnectReason::Cancelled
            }]
        );
        assert!(session.cancel_at(now).expect("second cancel").is_empty());
    }

    #[test]
    fn malformed_touch_is_rejected_before_tls_or_wire_use() {
        let mut session = session();
        session.state = SessionState::Running;
        assert!(
            session
                .send_at(
                    Command::Touch {
                        timestamp_micros: 1,
                        action: TouchAction::Down,
                        action_index: 0,
                        pointers: vec![TouchPointer {
                            x: 800,
                            y: 0,
                            id: 0
                        }],
                    },
                    Instant::now(),
                )
                .is_err()
        );
    }

    #[test]
    fn livi_audio_focus_release_maps_to_loss() {
        assert_eq!(audio_focus_state(1), 1);
        assert_eq!(audio_focus_state(2), 2);
        assert_eq!(audio_focus_state(3), 2);
        assert_eq!(audio_focus_state(4), 3);
        assert_eq!(audio_focus_state(0), 3);
    }

    #[test]
    fn tls_cleartext_fragmentation_is_bounded_and_ordered() {
        let mut assembler = TlsCleartextAssembler::default();
        assert!(
            assembler
                .push(
                    3,
                    FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_FIRST,
                    Some(5),
                    &[0, 1],
                )
                .expect("first")
                .is_none()
        );
        assert!(
            assembler
                .push(
                    3,
                    FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_MIDDLE,
                    None,
                    &[2, 3],
                )
                .expect("middle")
                .is_none()
        );
        assert_eq!(
            assembler
                .push(
                    3,
                    FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_LAST,
                    None,
                    &[4],
                )
                .expect("last")
                .expect("message"),
            [0, 1, 2, 3, 4]
        );
        assert!(
            assembler
                .push(
                    4,
                    FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_LAST,
                    None,
                    &[0],
                )
                .is_err()
        );
    }

    #[test]
    fn encrypted_fragment_overlap_and_message_type_changes_fail_closed() {
        let mut assembler = TlsCleartextAssembler::default();
        assembler
            .push(
                3,
                FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_FIRST,
                Some(4),
                &[1],
            )
            .expect("first");
        assert!(
            assembler
                .push(
                    3,
                    FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_FIRST,
                    Some(4),
                    &[2],
                )
                .is_err()
        );

        let mut assembler = TlsCleartextAssembler::default();
        assembler
            .push(
                3,
                FLAG_ENCRYPTED_SPECIFIC_BASE | FRAME_TYPE_FIRST,
                Some(4),
                &[1],
            )
            .expect("first");
        assert!(
            assembler
                .push(
                    3,
                    FLAG_ENCRYPTED_CONTROL_BASE | FRAME_TYPE_MIDDLE,
                    None,
                    &[2],
                )
                .is_err()
        );
    }

    #[test]
    fn single_tls_record_is_normalized_to_bulk_frame_type() {
        let mut session = session();
        session
            .write_tls_records(
                channel::CONTROL,
                FLAG_ENCRYPTED_CONTROL_BASE | FRAME_TYPE_FIRST,
                3,
                &[vec![0x17, 0x03, 0x03, 0x00, 0x00]],
            )
            .expect("single record");
        let frames = decode_written(&session.transport.take_written());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_ENCRYPTED_CONTROL);
        assert_eq!(frames[0].announced_total, None);
    }

    #[test]
    fn real_tls_inbound_three_record_message_reassembles_exactly() {
        let (mut session, mut phone, _) = running_session();
        let payload = (0..40 * 1024)
            .map(|index| u8::try_from(index % 251).expect("bounded byte"))
            .collect::<Vec<_>>();
        let mut expected = message::SERVICE_DISCOVERY_REQUEST.to_be_bytes().to_vec();
        expected.extend_from_slice(&payload);
        let encoded_frames = phone.encrypted_frame_list(
            channel::CONTROL,
            message::SERVICE_DISCOVERY_REQUEST,
            &payload,
        );
        assert!(encoded_frames.len() >= 3);

        let mut reassembled = None;
        for (index, encoded) in encoded_frames.iter().enumerate() {
            let frame = FrameDecoder::new()
                .push(encoded)
                .expect("physical frame")
                .pop()
                .expect("one frame");
            let expected_type = if index == 0 {
                FRAME_TYPE_FIRST
            } else if index + 1 == encoded_frames.len() {
                FRAME_TYPE_LAST
            } else {
                FRAME_TYPE_MIDDLE
            };
            assert_eq!(frame.flags, FLAG_ENCRYPTED_SPECIFIC_BASE | expected_type);
            assert_eq!(
                frame.announced_total,
                (index == 0).then_some(expected.len())
            );
            let progress = session
                .tls
                .as_mut()
                .expect("session TLS")
                .inject(&frame.payload)
                .expect("decrypt");
            let complete = session
                .tls_cleartext
                .push(
                    frame.channel_id,
                    frame.flags,
                    frame.announced_total,
                    &progress.plaintext,
                )
                .expect("reassembly");
            if complete.is_some() {
                assert!(reassembled.is_none());
                reassembled = complete;
            }
        }
        assert_eq!(reassembled.expect("complete cleartext"), expected);
    }

    #[test]
    fn real_tls_outbound_three_record_control_message_has_exact_framing() {
        let (mut session, mut phone, _) = running_session();
        let warmup_records = session
            .tls
            .as_mut()
            .expect("session TLS")
            .encrypt(&[1, 2, 3])
            .expect("warmup encrypt");
        assert_eq!(warmup_records.len(), 1);
        assert_eq!(phone.accept_tls(&warmup_records[0]), [1, 2, 3]);
        let cleartext = (0..40 * 1024)
            .map(|index| u8::try_from(index % 239).expect("bounded byte"))
            .collect::<Vec<_>>();
        let records = session
            .tls
            .as_mut()
            .expect("session TLS")
            .encrypt(&cleartext)
            .expect("encrypt");
        assert!(records.len() >= 3);
        session
            .write_tls_records(
                channel::CONTROL,
                FLAG_ENCRYPTED_CONTROL,
                cleartext.len(),
                &records,
            )
            .expect("write records");
        let frames = decode_written(&session.transport.take_written());
        assert_eq!(frames.len(), records.len());

        let mut decrypted = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            let expected_type = if index == 0 {
                FRAME_TYPE_FIRST
            } else if index + 1 == frames.len() {
                FRAME_TYPE_LAST
            } else {
                FRAME_TYPE_MIDDLE
            };
            assert_eq!(frame.flags, FLAG_ENCRYPTED_CONTROL_BASE | expected_type);
            assert_eq!(
                frame.announced_total,
                (index == 0).then_some(cleartext.len())
            );
            assert_eq!(frame.payload, records[index]);
            decrypted.extend_from_slice(&phone.accept_tls(&frame.payload));
        }
        assert_eq!(decrypted, cleartext);
    }

    #[test]
    fn session_config_rejects_invalid_head_unit_names() {
        for name in ["", "   ", "bad\0name", "bad\nname"] {
            let config = SessionConfig {
                head_unit_name: name.to_owned(),
                ..SessionConfig::default()
            };
            assert!(Session::new(MemoryTransport::default(), config).is_err());
        }
        let config = SessionConfig {
            head_unit_name: "x".repeat(crate::MAX_HEAD_UNIT_NAME_BYTES + 1),
            ..SessionConfig::default()
        };
        assert!(Session::new(MemoryTransport::default(), config).is_err());
    }

    #[test]
    fn session_config_rejects_invalid_video_density() {
        for density in [MIN_VIDEO_DENSITY_DPI - 1, MAX_VIDEO_DENSITY_DPI + 1] {
            let config = SessionConfig {
                video_density_dpi: density,
                ..SessionConfig::default()
            };
            assert!(Session::new(MemoryTransport::default(), config).is_err());
        }
    }

    #[test]
    fn protocol_native_media_timestamp_is_big_endian() {
        let (timestamp, payload) = media_payload(
            message::MEDIA_WITH_TIMESTAMP,
            &[0, 0, 0, 0, 0, 0, 0, 7, 1, 2],
        )
        .expect("media");
        assert_eq!(timestamp, Some(7));
        assert_eq!(payload, [1, 2]);
        assert!(media_payload(message::MEDIA_WITH_TIMESTAMP, &[0; 7]).is_err());
    }

    #[test]
    fn service_discovery_and_channel_open_use_control_channel() {
        let (mut session, mut phone, now) = discovered_session();
        let messages = drain_hu_messages(&mut session, &mut phone);
        assert!(messages.iter().any(|(channel_id, message_id, payload)| {
            *channel_id == channel::CONTROL
                && *message_id == message::SERVICE_DISCOVERY_RESPONSE
                && !payload.is_empty()
        }));
        assert!(
            messages
                .iter()
                .any(|(_, message_id, _)| { *message_id == message::PING_REQUEST })
        );

        let request = phone.encrypted_frames(
            channel::CONTROL,
            message::CHANNEL_OPEN_REQUEST,
            &ChannelOpenRequest {
                priority: Some(0),
                service_id: Some(i32::from(channel::VIDEO)),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&request, now + Duration::from_millis(2))
            .expect("channel open");
        let messages = drain_hu_messages(&mut session, &mut phone);
        let response = messages
            .iter()
            .find(|(channel_id, message_id, _)| {
                *channel_id == channel::CONTROL && *message_id == message::CHANNEL_OPEN_RESPONSE
            })
            .expect("open response");
        let status: crate::proto::StatusResponse =
            decode("open response", &response.2).expect("status");
        assert_eq!(status.status, STATUS_OK);
    }

    #[test]
    fn trace_records_channel_opens_and_the_last_phone_message() {
        let (mut session, mut phone, now) = discovered_session();
        drain_hu_messages(&mut session, &mut phone);
        let request = phone.encrypted_frames(
            channel::CONTROL,
            message::CHANNEL_OPEN_REQUEST,
            &ChannelOpenRequest {
                priority: Some(0),
                service_id: Some(i32::from(channel::SYSTEM_AUDIO)),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&request, now + Duration::from_millis(2))
            .expect("open");
        let trace = session.trace();
        assert_eq!(trace.opened_channels, [channel::SYSTEM_AUDIO]);
        assert_eq!(
            trace.last_message,
            Some((channel::CONTROL, message::CHANNEL_OPEN_REQUEST))
        );
        assert_eq!(trace.closed_in, None);
        assert_eq!(
            session.millis_since_last_inbound(now + Duration::from_millis(42)),
            Some(40)
        );
    }

    #[test]
    fn phone_audio_rejects_media_and_guidance_channel_opens() {
        let (mut session, mut phone, now) = discovered_session();
        session.config.audio_output = false;
        drain_hu_messages(&mut session, &mut phone);
        for channel_id in channel::PHONE_AUDIO {
            let request = phone.encrypted_frames(
                channel::CONTROL,
                message::CHANNEL_OPEN_REQUEST,
                &ChannelOpenRequest {
                    priority: Some(0),
                    service_id: Some(i32::from(channel_id)),
                }
                .encode_to_vec(),
            );
            assert!(
                session
                    .ingest_at(&request, now + Duration::from_millis(2))
                    .is_err()
            );
        }
    }

    #[test]
    fn bluetooth_pairing_requests_are_answered_with_the_bond_state() {
        let (mut session, mut phone, now) = discovered_session();
        session.config.bluetooth_address = Some("30:E3:D6:03:96:1E".to_owned());
        drain_hu_messages(&mut session, &mut phone);
        let open = phone.encrypted_frames(
            channel::CONTROL,
            message::CHANNEL_OPEN_REQUEST,
            &ChannelOpenRequest {
                priority: Some(0),
                service_id: Some(i32::from(channel::BLUETOOTH)),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&open, now + Duration::from_millis(2))
            .expect("Bluetooth channel open");
        drain_hu_messages(&mut session, &mut phone);
        let request = phone.encrypted_frames(
            channel::BLUETOOTH,
            message::BLUETOOTH_PAIRING_REQUEST,
            &BluetoothPairingRequest {
                phone_address: Some("F0:5C:77:12:34:56".to_owned()),
                pairing_method: Some(2),
            }
            .encode_to_vec(),
        );
        let events = session
            .ingest_at(&request, now + Duration::from_millis(3))
            .expect("pairing request");
        assert!(matches!(
            events.as_slice(),
            [Event::BluetoothPairing { phone_address: Some(address) }] if address == "F0:5C:77:12:34:56"
        ));
        assert!(
            drain_hu_messages(&mut session, &mut phone)
                .iter()
                .all(|(channel_id, _, _)| *channel_id != channel::BLUETOOTH)
        );
        session.answer_bluetooth_pairing(false).expect("answer");
        assert!(session.answer_bluetooth_pairing(false).is_err());
        session.send_bluetooth_passkey(4_217).expect("passkey");
        let messages = drain_hu_messages(&mut session, &mut phone);
        let response = messages
            .iter()
            .find(|(channel_id, message_id, _)| {
                *channel_id == channel::BLUETOOTH
                    && *message_id == message::BLUETOOTH_PAIRING_RESPONSE
            })
            .expect("pairing response");
        let response: crate::proto::BluetoothPairingResponse =
            decode("pairing response", &response.2).expect("response");
        assert_eq!(response.status, STATUS_OK);
        assert!(!response.already_paired);
        let authentication = messages
            .iter()
            .find(|(channel_id, message_id, _)| {
                *channel_id == channel::BLUETOOTH
                    && *message_id == message::BLUETOOTH_AUTHENTICATION_DATA
            })
            .expect("authentication data");
        let authentication: crate::proto::BluetoothAuthenticationData =
            decode("authentication data", &authentication.2).expect("authentication");
        assert_eq!(authentication.auth_data, "004217");
        let result = phone.encrypted_frames(
            channel::BLUETOOTH,
            message::BLUETOOTH_AUTHENTICATION_RESULT,
            &BluetoothAuthenticationResult { status: Some(0) }.encode_to_vec(),
        );
        let events = session
            .ingest_at(&result, now + Duration::from_millis(4))
            .expect("authentication result");
        assert!(matches!(
            events.as_slice(),
            [Event::BluetoothAuthenticated { status: Some(0) }]
        ));
    }

    #[test]
    fn bluetooth_channel_is_rejected_without_a_car_address() {
        let (mut session, mut phone, now) = discovered_session();
        drain_hu_messages(&mut session, &mut phone);
        let open = phone.encrypted_frames(
            channel::CONTROL,
            message::CHANNEL_OPEN_REQUEST,
            &ChannelOpenRequest {
                priority: Some(0),
                service_id: Some(i32::from(channel::BLUETOOTH)),
            }
            .encode_to_vec(),
        );
        assert!(
            session
                .ingest_at(&open, now + Duration::from_millis(2))
                .is_err()
        );
    }

    #[test]
    fn session_config_rejects_malformed_bluetooth_addresses() {
        let config = SessionConfig {
            bluetooth_address: Some("not-an-address".to_owned()),
            ..SessionConfig::default()
        };
        assert!(Session::new(MemoryTransport::default(), config).is_err());
    }

    #[test]
    fn night_mode_sensor_accepts_runtime_changes() {
        let (mut session, mut phone, now) = running_session();
        let request = phone.encrypted_frames(
            channel::SENSOR,
            message::SENSOR_START_REQUEST,
            &SensorRequest {
                sensor_type: Some(10),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&request, now + Duration::from_millis(3))
            .expect("night-mode sensor start");
        let initial = drain_hu_messages(&mut session, &mut phone);
        assert_eq!(session.night_mode(), Some(false));
        assert!(initial.iter().any(|(channel_id, message_id, payload)| {
            *channel_id == channel::SENSOR
                && *message_id == message::SENSOR_BATCH
                && payload == &[0x52, 0x02, 0x08, 0x00]
        }));

        assert!(session.set_night_mode(true).expect("night-mode update"));
        let update = drain_hu_messages(&mut session, &mut phone);
        assert_eq!(session.night_mode(), Some(true));
        assert!(update.iter().any(|(channel_id, message_id, payload)| {
            *channel_id == channel::SENSOR
                && *message_id == message::SENSOR_BATCH
                && payload == &[0x52, 0x02, 0x08, 0x01]
        }));

        assert!(
            session
                .set_night_mode(true)
                .expect("idempotent night-mode update")
        );
        assert!(drain_hu_messages(&mut session, &mut phone).is_empty());
    }

    #[test]
    fn night_mode_update_waits_for_the_phone_sensor() {
        let mut session = session();
        assert_eq!(session.night_mode(), None);
        assert!(!session.set_night_mode(true).expect("inactive sensor"));
        assert!(session.transport.written.is_empty());
    }

    fn running_session() -> (Session<MemoryTransport>, PhoneTls, Instant) {
        let (mut session, mut phone, now) = discovered_session();
        let _ = drain_hu_messages(&mut session, &mut phone);
        let setup = phone.encrypted_frames(
            channel::VIDEO,
            message::AV_SETUP_REQUEST,
            &Setup {
                media_codec_type: Some(MEDIA_CODEC_H264_BASELINE),
            }
            .encode_to_vec(),
        );
        let events = session
            .ingest_at(&setup, now + Duration::from_millis(2))
            .expect("video setup");
        assert_eq!(events, [Event::Connected]);
        assert_eq!(session.state(), SessionState::Running);
        let setup_messages = drain_hu_messages(&mut session, &mut phone);
        let response = setup_messages
            .iter()
            .find(|(_, message_id, _)| *message_id == message::AV_SETUP_RESPONSE)
            .expect("setup response");
        let response: crate::proto::SetupResponse =
            decode("setup response", &response.2).expect("setup response proto");
        assert_eq!(response.media_status, 2);
        assert_eq!(response.max_unacked, Some(VIDEO_MAX_UNACKED));
        assert_eq!(response.configs, [0]);
        assert!(setup_messages.iter().any(|(_, message_id, payload)| {
            *message_id == message::VIDEO_FOCUS_INDICATION && payload == &[0x08, 0x01]
        }));
        (session, phone, now)
    }

    #[test]
    fn video_and_audio_frames_ack_the_negotiated_session_ids() {
        let (mut session, mut phone, now) = running_session();
        let video_start = phone.encrypted_frames(
            channel::VIDEO,
            message::AV_START,
            &Start {
                session_id: Some(7),
                configuration_index: Some(0),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&video_start, now + Duration::from_millis(3))
            .expect("video start");
        let video = phone.encrypted_frames(
            channel::VIDEO,
            message::MEDIA_WITH_TIMESTAMP,
            &[0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 1, 0x67],
        );
        let events = session
            .ingest_at(&video, now + Duration::from_millis(4))
            .expect("video");
        assert_eq!(
            events,
            [Event::Video {
                timestamp: Some(9),
                annex_b: vec![0, 0, 0, 1, 0x67]
            }]
        );
        let ack_message = drain_hu_messages(&mut session, &mut phone)
            .into_iter()
            .find(|(_, message_id, _)| *message_id == message::AV_ACK)
            .expect("video ACK");
        let video_ack: Ack = decode("video ACK", &ack_message.2).expect("ACK");
        assert_eq!(video_ack.session_id, Some(7));
        assert_eq!(video_ack.ack, Some(1));

        let audio_setup = phone.encrypted_frames(
            channel::MEDIA_AUDIO,
            message::AV_SETUP_REQUEST,
            &Setup {
                media_codec_type: Some(MEDIA_CODEC_PCM),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&audio_setup, now + Duration::from_millis(5))
            .expect("audio setup");
        let audio_setup_response = drain_hu_messages(&mut session, &mut phone)
            .into_iter()
            .find(|(_, message_id, _)| *message_id == message::AV_SETUP_RESPONSE)
            .expect("audio setup response");
        let audio_setup_response: crate::proto::SetupResponse =
            decode("audio setup response", &audio_setup_response.2).expect("setup response proto");
        assert_eq!(audio_setup_response.max_unacked, Some(AUDIO_MAX_UNACKED));
        let audio_start = phone.encrypted_frames(
            channel::MEDIA_AUDIO,
            message::AV_START,
            &Start {
                session_id: Some(11),
                configuration_index: Some(0),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&audio_start, now + Duration::from_millis(6))
            .expect("audio start");
        let audio = phone.encrypted_frames(channel::MEDIA_AUDIO, message::MEDIA, &[1, 0, 2, 0]);
        let events = session
            .ingest_at(&audio, now + Duration::from_millis(7))
            .expect("audio");
        assert_eq!(
            events,
            [Event::Audio {
                channel: AudioChannel::Media,
                timestamp: None,
                pcm_s16le: vec![1, 0, 2, 0]
            }]
        );
        let ack_message = drain_hu_messages(&mut session, &mut phone)
            .into_iter()
            .find(|(_, message_id, _)| *message_id == message::AV_ACK)
            .expect("audio ACK");
        let audio_ack: Ack = decode("audio ACK", &ack_message.2).expect("ACK");
        assert_eq!(audio_ack.session_id, Some(11));
    }

    #[test]
    fn malformed_video_and_media_before_start_fail_closed() {
        let (mut session, mut phone, now) = running_session();
        let malformed = phone.encrypted_frames(channel::VIDEO, message::MEDIA, &[1, 2, 3]);
        assert!(
            session
                .ingest_at(&malformed, now + Duration::from_millis(3))
                .is_err()
        );
    }

    #[test]
    fn focus_release_and_keyframe_recovery_match_livi() {
        let (mut session, mut phone, now) = running_session();
        let focus = phone.encrypted_frames(
            channel::CONTROL,
            message::AUDIO_FOCUS_REQUEST,
            &AudioFocusRequest {
                audio_focus_type: Some(4),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&focus, now + Duration::from_millis(3))
            .expect("focus");
        let messages = drain_hu_messages(&mut session, &mut phone);
        assert!(messages.iter().any(|(_, message_id, payload)| {
            *message_id == message::AUDIO_FOCUS_RESPONSE && payload == &[0x08, 0x03]
        }));

        let request_at = now + Duration::from_millis(4);
        session
            .send_at(Command::RequestKeyFrame, request_at)
            .expect("keyframe request");
        let native = drain_hu_messages(&mut session, &mut phone);
        assert!(native.iter().any(|(_, message_id, payload)| {
            *message_id == message::VIDEO_FOCUS_INDICATION && payload == &[0x08, 0x02]
        }));
        session
            .tick(request_at + Duration::from_millis(59))
            .expect("early tick");
        assert!(session.transport.written.is_empty());
        session
            .tick(request_at + Duration::from_millis(60))
            .expect("due tick");
        let projected = drain_hu_messages(&mut session, &mut phone);
        assert!(projected.iter().any(|(_, message_id, payload)| {
            *message_id == message::VIDEO_FOCUS_INDICATION && payload == &[0x08, 0x01]
        }));
    }

    #[test]
    fn routine_voice_battery_and_focus_notifications_do_not_tear_down() {
        let (mut session, mut phone, now) = running_session();
        for (status, active) in [(1, true), (2, false)] {
            let notification = phone.encrypted_frames(
                channel::CONTROL,
                message::VOICE_SESSION_NOTIFICATION,
                &VoiceSessionNotification {
                    status: Some(status),
                }
                .encode_to_vec(),
            );
            let events = session
                .ingest_at(&notification, now + Duration::from_millis(3))
                .expect("voice session notification");
            assert_eq!(events, [Event::VoiceSession { active }]);
            assert_eq!(session.state(), SessionState::Running);
        }

        let battery = phone.encrypted_frames(
            channel::CONTROL,
            message::BATTERY_STATUS_NOTIFICATION,
            &BatteryStatusNotification {
                battery_level: Some(73),
                time_remaining_seconds: Some(3_600),
                critical: Some(false),
            }
            .encode_to_vec(),
        );
        let events = session
            .ingest_at(&battery, now + Duration::from_millis(4))
            .expect("battery notification");
        assert_eq!(
            events,
            [Event::Battery {
                level: 73,
                critical: false,
                time_remaining_seconds: Some(3_600)
            }]
        );

        let video_focus = phone.encrypted_frames(
            channel::VIDEO,
            message::VIDEO_FOCUS_INDICATION,
            &VideoFocusNotification {
                focus: Some(1),
                unsolicited: Some(false),
            }
            .encode_to_vec(),
        );
        assert!(
            session
                .ingest_at(&video_focus, now + Duration::from_millis(5))
                .expect("video focus indication")
                .is_empty()
        );

        let open_response = phone.encrypted_frames(
            channel::CONTROL,
            message::CHANNEL_OPEN_RESPONSE,
            &crate::proto::StatusResponse { status: STATUS_OK }.encode_to_vec(),
        );
        assert!(
            session
                .ingest_at(&open_response, now + Duration::from_millis(6))
                .expect("channel open response")
                .is_empty()
        );
        assert_eq!(session.state(), SessionState::Running);
    }

    #[test]
    fn unknown_voice_status_and_invalid_battery_fail_closed() {
        let (mut session, mut phone, now) = running_session();
        let voice = phone.encrypted_frames(
            channel::CONTROL,
            message::VOICE_SESSION_NOTIFICATION,
            &VoiceSessionNotification { status: Some(9) }.encode_to_vec(),
        );
        assert!(
            session
                .ingest_at(&voice, now + Duration::from_millis(3))
                .is_err()
        );

        let (mut session, mut phone, now) = running_session();
        let battery = phone.encrypted_frames(
            channel::CONTROL,
            message::BATTERY_STATUS_NOTIFICATION,
            &BatteryStatusNotification {
                battery_level: Some(101),
                time_remaining_seconds: None,
                critical: None,
            }
            .encode_to_vec(),
        );
        assert!(
            session
                .ingest_at(&battery, now + Duration::from_millis(3))
                .is_err()
        );
    }

    #[test]
    fn touch_button_and_rotary_commands_encode_typed_input_reports() {
        let (mut session, mut phone, now) = running_session();
        session
            .send_at(
                Command::Touch {
                    timestamp_micros: 10,
                    action: TouchAction::Down,
                    action_index: 0,
                    pointers: vec![TouchPointer { x: 7, y: 8, id: 0 }],
                },
                now,
            )
            .expect("touch");
        session
            .send_at(
                Command::MediaKey {
                    timestamp_micros: 11,
                    key: MediaKey::Next,
                    down: true,
                },
                now,
            )
            .expect("media key");
        session
            .send_at(
                Command::Button {
                    timestamp_micros: 12,
                    button: Button::Search,
                    down: true,
                    longpress: true,
                },
                now,
            )
            .expect("voice-assistant key");
        session
            .send_at(
                Command::Rotary {
                    timestamp_micros: 13,
                    direction: -1,
                },
                now,
            )
            .expect("rotary");
        let messages = drain_hu_messages(&mut session, &mut phone);
        let reports = messages
            .into_iter()
            .filter(|(_, message_id, _)| *message_id == message::INPUT_REPORT)
            .map(|(_, _, payload)| {
                crate::proto::InputReport::decode(payload.as_slice()).expect("input report")
            })
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 4);
        assert_eq!(
            reports[0].touch_event.as_ref().expect("touch").pointer_data[0].x,
            7
        );
        assert_eq!(
            reports[1].key_event.as_ref().expect("key").keys[0].keycode,
            87
        );
        assert_eq!(
            reports[2].key_event.as_ref().expect("voice key").keys[0].keycode,
            84
        );
        assert!(
            reports[2].key_event.as_ref().expect("voice key").keys[0]
                .longpress
                .is_some_and(|longpress| longpress)
        );
        assert_eq!(
            reports[3].relative_event.as_ref().expect("rotary").data[0].delta,
            -1
        );
    }

    #[test]
    fn microphone_flow_control_queues_until_matching_ack() {
        let (mut session, mut phone, now) = running_session();
        let open = phone.encrypted_frames(
            channel::MICROPHONE,
            message::MICROPHONE_OPEN_REQUEST,
            &MicrophoneRequest {
                open: Some(true),
                anc_enabled: None,
                echo_cancellation_enabled: None,
                max_unacked: Some(1),
            }
            .encode_to_vec(),
        );
        let events = session
            .ingest_at(&open, now + Duration::from_millis(3))
            .expect("mic open");
        assert_eq!(
            events,
            [Event::Microphone {
                open: true,
                sample_rate: 16_000,
                channels: 1
            }]
        );
        let open_messages = drain_hu_messages(&mut session, &mut phone);
        assert!(
            open_messages
                .iter()
                .any(|(_, message_id, _)| { *message_id == message::MICROPHONE_OPEN_RESPONSE })
        );
        assert!(
            !open_messages
                .iter()
                .any(|(_, message_id, _)| *message_id == message::AV_START)
        );

        let repeated_open = phone.encrypted_frames(
            channel::MICROPHONE,
            message::MICROPHONE_OPEN_REQUEST,
            &MicrophoneRequest {
                open: Some(true),
                anc_enabled: None,
                echo_cancellation_enabled: None,
                max_unacked: Some(1),
            }
            .encode_to_vec(),
        );
        assert_eq!(
            session
                .ingest_at(&repeated_open, now + Duration::from_millis(4))
                .expect("repeated mic open"),
            [Event::Microphone {
                open: true,
                sample_rate: 16_000,
                channels: 1
            }]
        );
        let _ = drain_hu_messages(&mut session, &mut phone);
        session
            .send_at(
                Command::MicrophonePcm {
                    timestamp: 1,
                    pcm_s16le: vec![1, 0],
                },
                now,
            )
            .expect("first PCM");
        let first = drain_hu_messages(&mut session, &mut phone);
        assert!(
            first
                .iter()
                .any(|(_, message_id, _)| { *message_id == message::MEDIA_WITH_TIMESTAMP })
        );
        session
            .send_at(
                Command::MicrophonePcm {
                    timestamp: 2,
                    pcm_s16le: vec![2, 0],
                },
                now,
            )
            .expect("queued PCM");
        assert!(session.transport.written.is_empty());
        let acknowledgment = phone.encrypted_frames(
            channel::MICROPHONE,
            message::AV_ACK,
            &Ack {
                session_id: Some(MICROPHONE_SESSION_ID),
                ack: Some(1),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&acknowledgment, now + Duration::from_millis(4))
            .expect("mic ACK");
        let second = drain_hu_messages(&mut session, &mut phone);
        let payload = second
            .iter()
            .find(|(_, message_id, _)| *message_id == message::MEDIA_WITH_TIMESTAMP)
            .map(|(_, _, payload)| payload)
            .expect("queued PCM sent");
        assert_eq!(&payload[..8], &2_u64.to_be_bytes());

        let batched_acknowledgment = phone.encrypted_frames(
            channel::MICROPHONE,
            message::AV_ACK,
            &Ack {
                session_id: Some(0),
                ack: Some(2),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&batched_acknowledgment, now + Duration::from_millis(5))
            .expect("batched mic ACK");
        let late_acknowledgment = phone.encrypted_frames(
            channel::MICROPHONE,
            message::AV_ACK,
            &Ack {
                session_id: None,
                ack: None,
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&late_acknowledgment, now + Duration::from_millis(6))
            .expect("late mic ACK");

        let close = phone.encrypted_frames(
            channel::MICROPHONE,
            message::MICROPHONE_OPEN_REQUEST,
            &MicrophoneRequest {
                open: Some(false),
                anc_enabled: None,
                echo_cancellation_enabled: None,
                max_unacked: Some(1),
            }
            .encode_to_vec(),
        );
        session
            .ingest_at(&close, now + Duration::from_millis(7))
            .expect("mic close");
        let _ = drain_hu_messages(&mut session, &mut phone);
        session
            .send_at(
                Command::MicrophonePcm {
                    timestamp: 3,
                    pcm_s16le: vec![3, 0],
                },
                now,
            )
            .expect("late PCM after mic close");
        assert!(session.transport.written.is_empty());
    }

    #[test]
    fn byebye_ack_and_timeout_both_close_once() {
        let (mut session, mut phone, now) = running_session();
        session
            .request_shutdown_at(1, now + Duration::from_millis(3))
            .expect("shutdown");
        assert_eq!(session.state(), SessionState::Closing);
        let messages = drain_hu_messages(&mut session, &mut phone);
        assert!(
            messages
                .iter()
                .any(|(_, message_id, _)| { *message_id == message::SHUTDOWN_REQUEST })
        );
        let response = phone.encrypted_frames(channel::CONTROL, message::SHUTDOWN_RESPONSE, &[]);
        let events = session
            .ingest_at(&response, now + Duration::from_millis(4))
            .expect("shutdown ACK");
        assert_eq!(
            events,
            [Event::Disconnected {
                reason: DisconnectReason::LocalShutdown
            }]
        );

        let (mut fallback, mut phone, now) = running_session();
        fallback
            .request_shutdown_at(1, now)
            .expect("fallback shutdown");
        let _ = drain_hu_messages(&mut fallback, &mut phone);
        let events = fallback
            .tick(now + Duration::from_secs(1))
            .expect("fallback tick");
        assert_eq!(
            events,
            [Event::Disconnected {
                reason: DisconnectReason::LocalShutdown
            }]
        );
        assert!(
            fallback
                .tick(now + Duration::from_secs(2))
                .expect("repeat")
                .is_empty()
        );
    }
}
