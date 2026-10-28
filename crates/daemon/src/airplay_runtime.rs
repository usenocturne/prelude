#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::{self, Read, Write},
    net::{Ipv6Addr, Shutdown, SocketAddr, SocketAddrV6, TcpListener, UdpSocket},
    num::NonZeroU16,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use plist::{Dictionary, Value};
use prelude_net::{
    AccessoryIdentity, AirPlayReceiver, ControlCipher, ControlKeys, ControlTransport,
    DataStreamDirection, EventChannelCipher, EventChannelCodec, EventChannelKeys,
    EventChannelMessage, InfoPayload, MfiSigner, PlistLimits, PortProvider, ReceiverEncryptionKeys,
    ReceiverEvent, ReceiverOutput, ReceiverState, RtspCodec, RtspLimits, RtspRequest, RtspResponse,
    SessionPorts, StreamDescriptor, StreamKey, StreamPorts, TimingDescriptor, encode_response,
};
#[cfg(any(test, target_os = "linux"))]
use prelude_net::{PairingStore, ReceiverConfig};
use prelude_usb_carkit::CancellationToken;
use tracing::info;
use zeroize::Zeroizing;

use platform_superbird::{
    AmbientLightState, KnobInputButton, LocalInputAction, PlatformInputEvent,
    PlatformInputEventKind, RotaryDirection, TouchInputPhase,
};
#[cfg(target_os = "linux")]
use platform_superbird::{LinuxInputBackend, LinuxInputConfig, LinuxInputTerminal};
use prelude_control::{
    AirPlayEvent, ControlEvent, ControlEventDecoder, InputWireSession, KnobButton, MediaButton,
    MonotonicTimestamp, TelephonyButton, TouchAction,
};

#[cfg(all(target_os = "linux", feature = "linux-media"))]
use crate::media_runtime::{LiveMediaRuntime, MediaCapabilities, MediaController};
#[cfg(target_os = "linux")]
use crate::shared_mfi::SharedMfi;
use crate::{
    AirPlayConfig, AudioRuntimeSnapshot, Iap2Config, MediaConfig, PlatformConfig, PresetAction,
    PresetBindings,
    airplay_data_channel::{DataChannels, Iap2Tunnel, SharedIap2Tunnel},
    first_video::{FirstVideoAction, FirstVideoWatchdog},
    iap2_runtime::ReceiverStartMetadata,
};

#[cfg(all(test, not(all(target_os = "linux", feature = "linux-media"))))]
type RecordedDuckingEvents = Arc<Mutex<Vec<(bool, Option<f64>, u32)>>>;

#[cfg(not(all(target_os = "linux", feature = "linux-media")))]
#[derive(Debug, Clone)]
struct MediaController {
    #[cfg(test)]
    mute_toggle_attempts: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    ducking_events: RecordedDuckingEvents,
}

#[cfg(not(all(target_os = "linux", feature = "linux-media")))]
impl MediaController {
    #[cfg(test)]
    fn responding_for_test() -> Self {
        Self {
            mute_toggle_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            ducking_events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn audio_feedback(&self) -> Result<Vec<prelude_net::AudioFeedbackStream>, String> {
        Ok(Vec::new())
    }

    fn screen_frames_received(&self) -> u64 {
        0
    }

    fn set_peer(&self, _peer: SocketAddrV6) -> Result<(), String> {
        Ok(())
    }

    fn start_timing(
        &self,
        _descriptor: TimingDescriptor,
        _socket: UdpSocket,
    ) -> Result<(), String> {
        Ok(())
    }

    fn start_stream(
        &self,
        _descriptor: StreamDescriptor,
        _sockets: OwnedMediaSockets,
        _output_key: Zeroizing<[u8; 32]>,
        _input_key: Option<Zeroizing<[u8; 32]>>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn record(&self) -> Result<bool, String> {
        Ok(true)
    }

    fn set_ducking(
        &self,
        _active: bool,
        _main_linear_gain: Option<f64>,
        _duration_ms: u32,
    ) -> Result<(), String> {
        #[cfg(test)]
        self.ducking_events
            .lock()
            .map_err(|_| "ducking event lock is poisoned".to_owned())?
            .push((_active, _main_linear_gain, _duration_ms));
        Ok(())
    }

    #[cfg(test)]
    fn ducking_events(&self) -> Vec<(bool, Option<f64>, u32)> {
        self.ducking_events
            .lock()
            .map_or_else(|_| Vec::new(), |events| events.clone())
    }

    fn toggle_mute(&self) -> Result<(), String> {
        #[cfg(test)]
        self.mute_toggle_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    #[cfg(test)]
    fn mute_toggle_attempt_count(&self) -> usize {
        self.mute_toggle_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn flush(&self, _point: prelude_net::FlushPoint) -> Result<(), String> {
        Ok(())
    }

    fn teardown(&self, _streams: Vec<StreamKey>) -> Result<(), String> {
        Ok(())
    }
}

const MAXIMUM_SETUP_CODE_FILE_BYTES: usize = 64;
const MAXIMUM_PREBOUND_STREAMS_PER_KIND: usize = 16;
const MAXIMUM_UNIQUE_PORT_BIND_ATTEMPTS: usize = 1024;
const CARPLAY_FEATURES: u64 = 0x0061_5653_aee2;
const CARPLAY_AUDIO_FEATURES: u64 = 0x0100_0454_0a00;
const CARPLAY_FEATURES_NO_AUDIO: u64 = CARPLAY_FEATURES & !CARPLAY_AUDIO_FEATURES;
const MAIN_DISPLAY_UUID: &str = "b7e6c5a0-1111-4000-8000-000000000001";
const DISPLAY_WIDTH_PIXELS: u64 = 800;
const DISPLAY_HEIGHT_PIXELS: u64 = 480;
const CARPLAY_SOURCE_VERSION: &str = "320.17";
const MAXIMUM_PENDING_EVENT_RESPONSES: usize = 256;
const RECORD_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(15);
const CONTROL_SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const TENTATIVE_ADDRESS_WAIT: Duration = Duration::from_secs(3);
const TENTATIVE_ADDRESS_POLL: Duration = Duration::from_millis(100);

fn bind_once_the_address_settles(bind_address: &ScopedBindAddress) -> io::Result<TcpListener> {
    let deadline = Instant::now() + TENTATIVE_ADDRESS_WAIT;
    loop {
        match bind_address.tcp_listener() {
            Err(error)
                if error.kind() == io::ErrorKind::AddrNotAvailable && Instant::now() < deadline =>
            {
                thread::sleep(TENTATIVE_ADDRESS_POLL);
            }
            result => return result,
        }
    }
}

pub(crate) enum EventWorkerCommand {
    SetNightMode {
        enabled: Option<bool>,
        completed: mpsc::SyncSender<Result<(), String>>,
    },
    ForceKeyFrame,
    IapSendMessage(Vec<u8>),
}
const PCM_VOICE_FORMATS: u64 = 0x03fc;
const PCM_MEDIA_48K_FORMATS: u64 = 0xc000;
const PCM_MONO_48K_FORMATS: u64 = 0x4154;
const OPUS_MONO_FORMATS: u64 = 0x7000_0000;
const AAC_LC_48K_STEREO_FORMAT: u64 = 0x0080_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AirPlayStage {
    Listening,
    Connected,
    EncryptedControl,
    MfiAuthenticated,
    SessionConfigured,
    StreamsConfigured,
    Recording,
    Failed,
    Stopped,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-media")))]
impl AirPlayStage {
    pub(crate) const fn display_status(
        self,
        listening: crate::ConnectionStep,
    ) -> crate::DisplayStatus {
        use crate::{ConnectionStep, DisplayStatus};

        let step = match self {
            Self::Listening => listening,
            Self::Connected => ConnectionStep::VerifyingPairing,
            Self::EncryptedControl => ConnectionStep::Authenticating,
            Self::MfiAuthenticated => ConnectionStep::SettingUpSession,
            Self::SessionConfigured => ConnectionStep::SettingUpStreams,
            Self::StreamsConfigured => ConnectionStep::StartingStream,
            Self::Recording => return DisplayStatus::Connected,
            Self::Failed | Self::Stopped => return DisplayStatus::Connecting(None),
        };
        DisplayStatus::Connecting(Some(step))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AirPlayProgress {
    pub stage: AirPlayStage,
    pub event_count: u64,
    pub negotiated_streams: usize,
    pub event_channel_ready: bool,
    pub input_ready: bool,
    pub input_event_count: u64,
    pub local_event_count: u64,
    pub media_ready: bool,
    pub audio_ready: bool,
    pub microphone_ready: bool,
    pub media_recording: bool,
    pub bluetooth_handoff_requested: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct ReceiverMediaClaims {
    screen: bool,
    audio: bool,
    microphone: bool,
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
impl From<MediaCapabilities> for ReceiverMediaClaims {
    fn from(capabilities: MediaCapabilities) -> Self {
        Self {
            screen: capabilities.screen,
            audio: capabilities.audio,
            microphone: capabilities.microphone,
        }
    }
}

fn initial_airplay_progress(media: ReceiverMediaClaims, input_declared: bool) -> AirPlayProgress {
    AirPlayProgress {
        stage: AirPlayStage::Listening,
        event_count: 0,
        negotiated_streams: 0,
        event_channel_ready: false,
        input_ready: input_declared,
        input_event_count: 0,
        local_event_count: 0,
        media_ready: media.screen,
        audio_ready: media.audio,
        microphone_ready: false,
        media_recording: false,
        bluetooth_handoff_requested: false,
    }
}

type SharedProgress = Arc<Mutex<AirPlayProgress>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputTerminal {
    Cancelled,
    DeviceUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveInputBatch {
    events: Vec<PlatformInputEvent>,
    terminal: Option<InputTerminal>,
}

trait LiveInputSource: std::fmt::Debug + Send {
    fn poll(&mut self) -> Result<LiveInputBatch, String>;
    fn cancel(&mut self) -> Result<LiveInputBatch, String>;
}

#[cfg(target_os = "linux")]
impl LiveInputSource for LinuxInputBackend {
    fn poll(&mut self) -> Result<LiveInputBatch, String> {
        let batch = LinuxInputBackend::poll(self)
            .map_err(|error| format!("polling the Superbird input backend failed: {error}"))?;
        Ok(LiveInputBatch {
            events: batch.events,
            terminal: batch.terminal.map(|terminal| match terminal {
                LinuxInputTerminal::Cancelled => InputTerminal::Cancelled,
                LinuxInputTerminal::DeviceUnplugged(_)
                | LinuxInputTerminal::DeviceReadFailed(_) => InputTerminal::DeviceUnavailable,
            }),
        })
    }

    fn cancel(&mut self) -> Result<LiveInputBatch, String> {
        let batch = LinuxInputBackend::cancel(self)
            .map_err(|error| format!("cancelling the Superbird input backend failed: {error}"))?;
        Ok(LiveInputBatch {
            events: batch.events,
            terminal: batch.terminal.map(|_| InputTerminal::Cancelled),
        })
    }
}

#[derive(Debug, Clone)]
struct ScopedBindAddress {
    address: Ipv6Addr,
    scope_id: u32,
}

impl ScopedBindAddress {
    fn production(address: Ipv6Addr, scope_id: u32) -> Result<Self, String> {
        if !address.is_unicast_link_local() || scope_id == 0 {
            return Err(
                "AirPlay requires a verified link-local IPv6 address with an interface scope"
                    .to_owned(),
            );
        }
        Ok(Self { address, scope_id })
    }

    #[cfg(test)]
    fn test_loopback() -> Self {
        Self {
            address: Ipv6Addr::LOCALHOST,
            scope_id: 0,
        }
    }

    fn socket_address(&self) -> SocketAddrV6 {
        SocketAddrV6::new(self.address, 0, 0, self.scope_id)
    }

    fn tcp_listener(&self) -> io::Result<TcpListener> {
        let listener = TcpListener::bind(self.socket_address())?;
        listener.set_nonblocking(true)?;
        self.verify_local(listener.local_addr()?)?;
        Ok(listener)
    }

    fn udp_socket(&self) -> io::Result<UdpSocket> {
        let socket = UdpSocket::bind(self.socket_address())?;
        socket.set_nonblocking(true)?;
        self.verify_local(socket.local_addr()?)?;
        Ok(socket)
    }

    fn verify_local(&self, address: SocketAddr) -> io::Result<()> {
        match address {
            SocketAddr::V6(address)
                if *address.ip() == self.address && address.scope_id() == self.scope_id =>
            {
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "socket was not bound to the requested scoped IPv6 interface",
            )),
        }
    }

    fn accepts_peer(&self, peer: SocketAddr) -> bool {
        matches!(
            peer,
            SocketAddr::V6(peer)
                if peer.ip().is_unicast_link_local() && peer.scope_id() == self.scope_id
        )
    }
}

enum BoundStreamSockets {
    Screen(TcpListener),
    Audio(UdpSocket, UdpSocket),
    DataChannel(TcpListener),
}

impl std::fmt::Debug for BoundStreamSockets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Screen(_) => formatter.write_str("Screen([bound])"),
            Self::Audio(_, _) => formatter.write_str("Audio([bound], [bound])"),
            Self::DataChannel(_) => formatter.write_str("DataChannel([bound])"),
        }
    }
}

struct BoundPorts {
    bind_address: ScopedBindAddress,
    timing: Option<UdpSocket>,
    timing_port: NonZeroU16,
    event: Option<TcpListener>,
    event_port: NonZeroU16,
    keep_alive: UdpSocket,
    available_screen: VecDeque<TcpListener>,
    available_audio: VecDeque<(UdpSocket, UdpSocket)>,
    streams: BTreeMap<StreamKey, BoundStreamSockets>,
    claimed_streams: BTreeSet<StreamKey>,
    used_stream_ports: BTreeSet<u16>,
}

impl std::fmt::Debug for BoundPorts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundPorts")
            .field("session_sockets_bound", &3)
            .field("available_screen", &self.available_screen.len())
            .field("available_audio", &self.available_audio.len())
            .field("allocated_streams", &self.streams.len())
            .field("event_listener_owned", &self.event.is_some())
            .finish()
    }
}

impl BoundPorts {
    fn bind(address: &ScopedBindAddress) -> io::Result<Self> {
        let timing = address.udp_socket()?;
        let event = address.tcp_listener()?;
        let keep_alive = address.udp_socket()?;
        let mut available_screen = VecDeque::with_capacity(MAXIMUM_PREBOUND_STREAMS_PER_KIND);
        let mut available_audio = VecDeque::with_capacity(MAXIMUM_PREBOUND_STREAMS_PER_KIND);
        let mut used_stream_ports = BTreeSet::new();
        for _ in 0..MAXIMUM_PREBOUND_STREAMS_PER_KIND {
            let screen = address.tcp_listener()?;
            used_stream_ports.insert(screen.local_addr()?.port());
            available_screen.push_back(screen);
            let audio = (address.udp_socket()?, address.udp_socket()?);
            used_stream_ports.insert(audio.0.local_addr()?.port());
            used_stream_ports.insert(audio.1.local_addr()?.port());
            available_audio.push_back(audio);
        }
        let timing_port = socket_port(timing.local_addr())
            .map_err(|error| io::Error::new(io::ErrorKind::AddrNotAvailable, error.to_string()))?;
        let event_port = socket_port(event.local_addr())
            .map_err(|error| io::Error::new(io::ErrorKind::AddrNotAvailable, error.to_string()))?;
        Ok(Self {
            bind_address: address.clone(),
            timing: Some(timing),
            timing_port,
            event: Some(event),
            event_port,
            keep_alive,
            available_screen,
            available_audio,
            streams: BTreeMap::new(),
            claimed_streams: BTreeSet::new(),
            used_stream_ports,
        })
    }

    fn take_event_listener(&mut self) -> io::Result<TcpListener> {
        self.event.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "AirPlay event listener was already claimed",
            )
        })
    }

    fn take_media_sockets(&mut self, key: StreamKey) -> io::Result<Option<OwnedMediaSockets>> {
        if self.claimed_streams.contains(&key) {
            return Ok(None);
        }
        let sockets = self.streams.get(&key).map(BoundStreamSockets::try_clone);
        let sockets = sockets.transpose()?;
        if sockets.is_some() {
            self.claimed_streams.insert(key);
        }
        Ok(sockets)
    }

    fn take_data_channel_listener(&mut self, key: StreamKey) -> io::Result<Option<TcpListener>> {
        if self.claimed_streams.contains(&key) {
            return Ok(None);
        }
        let Some(BoundStreamSockets::DataChannel(listener)) = self.streams.get(&key) else {
            return Ok(None);
        };
        let listener = listener.try_clone()?;
        self.claimed_streams.insert(key);
        Ok(Some(listener))
    }

    fn release_media_sockets(&mut self, keys: &[StreamKey]) -> io::Result<()> {
        for key in keys {
            let replacement = match self.streams.get(key) {
                Some(BoundStreamSockets::Screen(_)) => {
                    Some(BoundStreamSockets::Screen(self.bind_unique_tcp_listener()?))
                }
                Some(BoundStreamSockets::Audio(_, _)) => Some(BoundStreamSockets::Audio(
                    self.bind_unique_udp_socket()?,
                    self.bind_unique_udp_socket()?,
                )),
                Some(BoundStreamSockets::DataChannel(_)) => None,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "released AirPlay stream has no retained socket owner",
                    ));
                }
            };
            if !self.claimed_streams.remove(key) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "released AirPlay stream sockets were never claimed",
                ));
            }
            let old = self.streams.remove(key).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "released AirPlay stream socket owner disappeared",
                )
            })?;
            drop(old);
            match replacement {
                Some(BoundStreamSockets::Screen(listener)) => {
                    self.available_screen.push_back(listener);
                }
                Some(BoundStreamSockets::Audio(data, control)) => {
                    self.available_audio.push_back((data, control));
                }
                Some(BoundStreamSockets::DataChannel(_)) | None => {}
            }
        }
        Ok(())
    }

    fn bind_unique_tcp_listener(&mut self) -> io::Result<TcpListener> {
        for _ in 0..MAXIMUM_UNIQUE_PORT_BIND_ATTEMPTS {
            let listener = self.bind_address.tcp_listener()?;
            if self.used_stream_ports.insert(listener.local_addr()?.port()) {
                return Ok(listener);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "could not bind a never-advertised AirPlay TCP stream port",
        ))
    }

    fn bind_unique_udp_socket(&mut self) -> io::Result<UdpSocket> {
        for _ in 0..MAXIMUM_UNIQUE_PORT_BIND_ATTEMPTS {
            let socket = self.bind_address.udp_socket()?;
            if self.used_stream_ports.insert(socket.local_addr()?.port()) {
                return Ok(socket);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "could not bind a never-advertised AirPlay UDP stream port",
        ))
    }
}

impl PortProvider for BoundPorts {
    fn session_ports(&mut self, timing: &TimingDescriptor) -> prelude_net::Result<SessionPorts> {
        Ok(SessionPorts {
            timing_port: self.timing_port,
            event_port: self.event_port,
            keep_alive_port: timing
                .low_power_keepalive
                .then(|| socket_port(self.keep_alive.local_addr()))
                .transpose()?,
        })
    }

    fn stream_ports(&mut self, stream: &StreamDescriptor) -> prelude_net::Result<StreamPorts> {
        let key = stream.key();
        if let Some(sockets) = self.streams.get(&key) {
            return stream_socket_ports(sockets);
        }
        let sockets = match stream {
            StreamDescriptor::Screen(_) => {
                BoundStreamSockets::Screen(self.available_screen.pop_front().ok_or(
                    prelude_net::Error::NotReady {
                        component: "AirPlay screen socket pool",
                        state: "prebound_screen_ports_exhausted",
                    },
                )?)
            }
            StreamDescriptor::Audio(_) => {
                let (data, control) =
                    self.available_audio
                        .pop_front()
                        .ok_or(prelude_net::Error::NotReady {
                            component: "AirPlay audio socket pool",
                            state: "prebound_audio_ports_exhausted",
                        })?;
                BoundStreamSockets::Audio(data, control)
            }
            StreamDescriptor::DataChannel(_) => BoundStreamSockets::DataChannel(
                self.bind_unique_tcp_listener().map_err(|error| {
                    prelude_net::Error::InvalidRtsp(format!(
                        "binding the AirPlay data channel port failed: {error}"
                    ))
                })?,
            ),
        };
        let ports = stream_socket_ports(&sockets)?;
        self.streams.insert(key, sockets);
        Ok(ports)
    }
}

#[derive(Clone)]
struct SharedBoundPorts(Arc<Mutex<BoundPorts>>);

impl std::fmt::Debug for SharedBoundPorts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("SharedBoundPorts")
            .field(&self.0.lock().map(|ports| format!("{ports:?}")))
            .finish()
    }
}

impl SharedBoundPorts {
    fn new(ports: BoundPorts) -> Self {
        Self(Arc::new(Mutex::new(ports)))
    }

    fn take_event_listener(&self) -> io::Result<TcpListener> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("AirPlay socket owner lock is poisoned"))?
            .take_event_listener()
    }

    pub(crate) fn take_timing_socket(&self) -> io::Result<Option<UdpSocket>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| io::Error::other("AirPlay socket owner lock is poisoned"))?
            .timing
            .take())
    }

    pub(crate) fn take_media_sockets(
        &self,
        key: StreamKey,
    ) -> io::Result<Option<OwnedMediaSockets>> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("AirPlay socket owner lock is poisoned"))?
            .take_media_sockets(key)
    }

    fn take_data_channel_listener(&self, key: StreamKey) -> io::Result<Option<TcpListener>> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("AirPlay socket owner lock is poisoned"))?
            .take_data_channel_listener(key)
    }

    fn release_media_sockets(&self, keys: &[StreamKey]) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("AirPlay socket owner lock is poisoned"))?
            .release_media_sockets(keys)
    }

    fn drain_keep_alive(&self) -> bool {
        let Ok(ports) = self.0.lock() else {
            return false;
        };
        let mut datagram = [0_u8; 64];
        let mut received = false;
        while ports.keep_alive.recv(&mut datagram).is_ok() {
            received = true;
        }
        received
    }
}

impl PortProvider for SharedBoundPorts {
    fn session_ports(&mut self, timing: &TimingDescriptor) -> prelude_net::Result<SessionPorts> {
        self.0
            .lock()
            .map_err(|_| prelude_net::Error::NotReady {
                component: "AirPlay socket owner",
                state: "poisoned",
            })?
            .session_ports(timing)
    }

    fn stream_ports(&mut self, stream: &StreamDescriptor) -> prelude_net::Result<StreamPorts> {
        self.0
            .lock()
            .map_err(|_| prelude_net::Error::NotReady {
                component: "AirPlay socket owner",
                state: "poisoned",
            })?
            .stream_ports(stream)
    }
}

pub(crate) enum OwnedMediaSockets {
    Screen(TcpListener),
    Audio { data: UdpSocket, control: UdpSocket },
}

impl std::fmt::Debug for OwnedMediaSockets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Screen(_) => formatter.write_str("OwnedMediaSockets::Screen([bound])"),
            Self::Audio { .. } => formatter.write_str("OwnedMediaSockets::Audio([bound])"),
        }
    }
}

impl BoundStreamSockets {
    fn try_clone(&self) -> io::Result<OwnedMediaSockets> {
        match self {
            Self::Screen(socket) => Ok(OwnedMediaSockets::Screen(socket.try_clone()?)),
            Self::Audio(data, control) => Ok(OwnedMediaSockets::Audio {
                data: data.try_clone()?,
                control: control.try_clone()?,
            }),
            Self::DataChannel(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AirPlay data channels carry no media",
            )),
        }
    }
}

fn socket_port(address: io::Result<SocketAddr>) -> prelude_net::Result<NonZeroU16> {
    let port = address
        .map_err(|error| {
            prelude_net::Error::InvalidRtsp(format!(
                "reading an already-bound receiver port failed: {error}"
            ))
        })?
        .port();
    NonZeroU16::new(port).ok_or(prelude_net::Error::NotReady {
        component: "AirPlay socket",
        state: "bound_port_is_zero",
    })
}

fn stream_socket_ports(sockets: &BoundStreamSockets) -> prelude_net::Result<StreamPorts> {
    match sockets {
        BoundStreamSockets::Screen(data) => Ok(StreamPorts::Screen {
            data_port: socket_port(data.local_addr())?,
        }),
        BoundStreamSockets::Audio(data, control) => Ok(StreamPorts::Audio {
            data_port: socket_port(data.local_addr())?,
            control_port: socket_port(control.local_addr())?,
        }),
        BoundStreamSockets::DataChannel(listener) => Ok(StreamPorts::DataChannel {
            data_port: socket_port(listener.local_addr())?,
        }),
    }
}

pub(crate) struct AirPlayRuntime {
    metadata: ReceiverStartMetadata,
    progress: SharedProgress,
    failure: Arc<Mutex<Option<String>>>,
    cancellation: CancellationToken,
    workers: Vec<JoinHandle<Result<(), String>>>,
    ports: SharedBoundPorts,
    iap2_tunnel: SharedIap2Tunnel,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    media: Option<LiveMediaRuntime>,
}

impl std::fmt::Debug for AirPlayRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AirPlayRuntime")
            .field("metadata", &self.metadata)
            .field("progress", &self.snapshot())
            .field(
                "has_failure",
                &self
                    .failure
                    .lock()
                    .map_or(true, |failure| failure.is_some()),
            )
            .field("workers_alive", &self.is_alive())
            .field("ports", &self.ports)
            .finish_non_exhaustive()
    }
}

impl AirPlayRuntime {
    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind(
        config: &AirPlayConfig,
        media_config: &MediaConfig,
        platform_config: &PlatformConfig,
        identity_config: &Iap2Config,
        state_dir: &Path,
        link_local: Ipv6Addr,
        interface_name: &str,
        host_mac: [u8; 6],
        signer: SharedMfi,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Result<Self, String> {
        if !config.enable_live_receiver {
            return Err("live AirPlay receiver is disabled by configuration".to_owned());
        }
        let setup_path = config
            .setup_code_file
            .as_deref()
            .ok_or_else(|| "AirPlay setup-code file is not configured".to_owned())?;
        let setup_code = read_setup_code(setup_path)?;
        let input_backend: Option<Box<dyn LiveInputSource>> = if config.enable_live_input {
            let input_config = LinuxInputConfig {
                poll_interval: Duration::from_millis(config.input_poll_interval_ms),
                ..LinuxInputConfig::default()
            };
            Some(Box::new(
                LinuxInputBackend::discover(input_config).map_err(|error| {
                    format!("opening the factual Superbird input backend failed: {error}")
                })?,
            ))
        } else {
            None
        };
        let input_declared = input_backend.is_some();
        let (event_command_sender, event_command_receiver) = mpsc::sync_channel(8);
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let event_display = display.clone();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let media = Some(LiveMediaRuntime::start(
            media_config,
            platform_config,
            !config.disable_audio_output,
            event_command_sender.clone(),
            display,
        )?);
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let media_claims = media
            .as_ref()
            .map_or_else(ReceiverMediaClaims::default, |media| {
                media.capabilities().into()
            });
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        let media_claims = {
            let _ = media_config;
            ReceiverMediaClaims::default()
        };
        let advertised_media_claims = ReceiverMediaClaims {
            screen: media_claims.screen,
            audio: media_claims.audio && !config.disable_audio_output,
            microphone: media_claims.microphone && !config.disable_audio_output,
        };
        let info = build_info_payload(
            config,
            identity_config,
            host_mac,
            input_declared,
            advertised_media_claims,
        )?;
        tracing::debug!(
            source_version = CARPLAY_SOURCE_VERSION,
            feature_bits = format_args!(
                "0x{:x}",
                if advertised_media_claims.audio {
                    CARPLAY_FEATURES
                } else {
                    CARPLAY_FEATURES_NO_AUDIO
                }
            ),
            displays = 1,
            hid_devices = if input_declared { 4 } else { 0 },
            audio_formats = if advertised_media_claims.audio { 9 } else { 0 },
            audio_latencies = if advertised_media_claims.audio { 9 } else { 0 },
            microphone = advertised_media_claims.microphone,
            dock = ?config.dock,
            "built AirPlay /info capability declaration"
        );
        let scope_id = nix::net::if_::if_nametoindex(interface_name).map_err(|error| {
            format!("resolving the selected NCM interface index failed: {error}")
        })?;
        let bind_address = ScopedBindAddress::production(link_local, scope_id)?;
        let listener = bind_once_the_address_settles(&bind_address).map_err(|error| {
            format!("binding the scoped AirPlay control listener failed: {error}")
        })?;
        let service_port = listener
            .local_addr()
            .map_err(|error| format!("reading the AirPlay control listener port failed: {error}"))?
            .port();
        if service_port == 0 {
            return Err("the AirPlay control listener returned port zero".to_owned());
        }
        let ports = SharedBoundPorts::new(BoundPorts::bind(&bind_address).map_err(|error| {
            format!("pre-binding AirPlay session/stream sockets failed: {error}")
        })?);
        let event_listener = ports.take_event_listener().map_err(|error| {
            format!("claiming the pre-bound AirPlay event listener failed: {error}")
        })?;
        let store = Arc::new(PairingStore::open(pairing_directory(state_dir)).map_err(
            |error| format!("opening persistent AirPlay pairing storage failed: {error}"),
        )?);
        let identity = Arc::new(
            store
                .load_or_create_identity()
                .map_err(|error| format!("loading persistent AirPlay identity failed: {error}"))?,
        );
        let metadata = receiver_start_metadata(service_port, host_mac, &identity);
        let receiver_config =
            ReceiverConfig::new(setup_code, identity.pairing_id().to_owned(), info)
                .with_pairing_timeout(Duration::from_millis(config.pairing_timeout_ms))
                .with_enabled_features(vec!["iAPChannel".to_owned(), "viewAreas".to_owned()]);
        let receiver = AirPlayReceiver::new(receiver_config, store, identity, signer)
            .map_err(|error| format!("constructing the AirPlay receiver failed: {error}"))?;
        let progress = Arc::new(Mutex::new(initial_airplay_progress(
            media_claims,
            input_declared,
        )));
        let failure = Arc::new(Mutex::new(None));
        let cancellation = CancellationToken::default();
        let accept_poll = Duration::from_millis(config.accept_poll_interval_ms);
        let io_timeout = Duration::from_millis(config.control_io_timeout_ms);
        let event_read_timeout = bounded_event_read_timeout(
            io_timeout,
            input_declared.then(|| Duration::from_millis(config.input_poll_interval_ms)),
        );
        let (event_key_sender, event_key_receiver) = mpsc::sync_channel(1);
        let event_progress = Arc::clone(&progress);
        let event_failure = Arc::clone(&failure);
        let event_cancellation = cancellation.clone();
        let event_bind_address = bind_address.clone();
        let event_ambient_light = ambient_light.clone();
        let event_presets = platform_config.presets.clone();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let event_media = media.as_ref().map(LiveMediaRuntime::controller);
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        let event_media = None;
        let event_worker = thread::Builder::new()
            .name("prelude-events".to_owned())
            .spawn(move || {
                let result = run_event_listener(
                    event_listener,
                    event_bind_address,
                    event_key_receiver,
                    event_command_receiver,
                    input_backend,
                    &event_progress,
                    &event_cancellation,
                    &event_ambient_light,
                    &event_presets,
                    accept_poll,
                    event_read_timeout,
                    io_timeout,
                    event_media,
                    #[cfg(all(target_os = "linux", feature = "linux-media"))]
                    event_display,
                );
                if let Err(error) = &result {
                    record_failure(&event_failure, error);
                    set_stage(&event_progress, AirPlayStage::Failed);
                }
                result
            })
            .map_err(|error| format!("starting the AirPlay event worker failed: {error}"))?;
        let control_progress = Arc::clone(&progress);
        let control_failure = Arc::clone(&failure);
        let control_cancellation = cancellation.clone();
        let control_ports = ports.clone();
        let iap2_tunnel = SharedIap2Tunnel::default();
        let control_iap2_tunnel = Arc::clone(&iap2_tunnel);
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let control_media = media.as_ref().map(LiveMediaRuntime::controller);
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        let control_media = None;
        let control_worker = match thread::Builder::new()
            .name("prelude-airplay".to_owned())
            .spawn(move || {
                let result = run_listener(
                    listener,
                    bind_address,
                    receiver,
                    control_ports,
                    event_key_sender,
                    event_command_sender,
                    &control_iap2_tunnel,
                    &control_progress,
                    &control_cancellation,
                    accept_poll,
                    io_timeout,
                    control_media,
                );
                if let Err(error) = &result {
                    record_failure(&control_failure, error);
                    set_stage(&control_progress, AirPlayStage::Failed);
                }
                result
            }) {
            Ok(worker) => worker,
            Err(error) => {
                cancellation.cancel();
                let _ = event_worker.join();
                return Err(format!(
                    "starting the AirPlay control worker failed: {error}"
                ));
            }
        };
        let runtime = Self {
            metadata,
            progress,
            failure,
            cancellation,
            workers: vec![control_worker, event_worker],
            ports,
            iap2_tunnel,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            media,
        };
        Ok(runtime)
    }

    pub(crate) fn take_iap2_tunnel(&self) -> Option<Iap2Tunnel> {
        self.iap2_tunnel.lock().ok()?.take()
    }

    pub(crate) fn start_metadata(&self) -> &ReceiverStartMetadata {
        &self.metadata
    }

    pub(crate) fn snapshot(&self) -> AirPlayProgress {
        let snapshot = self.progress.lock().map_or(
            AirPlayProgress {
                stage: AirPlayStage::Failed,
                event_count: 0,
                negotiated_streams: 0,
                event_channel_ready: false,
                input_ready: false,
                input_event_count: 0,
                local_event_count: 0,
                media_ready: false,
                audio_ready: false,
                microphone_ready: false,
                media_recording: false,
                bluetooth_handoff_requested: false,
            },
            |progress| *progress,
        );
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let snapshot = {
            let mut snapshot = snapshot;
            if let Some(media) = &self.media {
                let media = media.snapshot();
                snapshot.microphone_ready = media.microphone_ready;
                snapshot.media_recording = media.recording;
            }
            snapshot
        };
        snapshot
    }

    pub(crate) fn audio_output_missing(&self) -> bool {
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        return self
            .media
            .as_ref()
            .is_some_and(LiveMediaRuntime::audio_output_missing);
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        false
    }

    pub(crate) fn is_alive(&self) -> bool {
        let control_alive =
            !self.workers.is_empty() && self.workers.iter().all(|worker| !worker.is_finished());
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        return control_alive && self.media.as_ref().is_some_and(LiveMediaRuntime::is_alive);
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        control_alive
    }

    pub(crate) fn failure_detail(&self) -> Option<String> {
        let control = self.failure.lock().ok().and_then(|failure| failure.clone());
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        return control.or_else(|| {
            self.media
                .as_ref()
                .and_then(LiveMediaRuntime::failure_detail)
        });
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        control
    }

    pub(crate) fn audio_status(&self) -> AudioRuntimeSnapshot {
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        return self.media.as_ref().map_or_else(
            AudioRuntimeSnapshot::disabled,
            LiveMediaRuntime::audio_status,
        );
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        AudioRuntimeSnapshot::disabled()
    }

    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    pub(crate) fn play_audio_test_tone(&self) -> Result<(), String> {
        self.media
            .as_ref()
            .ok_or_else(|| "live media runtime is unavailable".to_owned())?
            .controller()
            .play_test_tone()
    }

    fn stop_inner(&mut self) -> Result<(), String> {
        self.cancellation.cancel();
        let mut result = Ok(());
        for worker in self.workers.drain(..) {
            let joined = worker
                .join()
                .map_err(|_| "AirPlay worker terminated unexpectedly".to_owned())
                .and_then(|result| result);
            if result.is_ok() {
                result = joined;
            }
        }
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        if let Some(media) = self.media.take() {
            let stopped = media.stop();
            if result.is_ok() {
                result = stopped;
            }
        }
        set_stage(&self.progress, AirPlayStage::Stopped);
        result
    }

    pub(crate) fn stop(mut self) -> Result<(), String> {
        self.stop_inner()
    }
}

impl Drop for AirPlayRuntime {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_listener<S: MfiSigner>(
    listener: TcpListener,
    bind_address: ScopedBindAddress,
    mut receiver: AirPlayReceiver<S>,
    mut ports: SharedBoundPorts,
    event_key_sender: mpsc::SyncSender<EventChannelKeys>,
    event_command_sender: mpsc::SyncSender<EventWorkerCommand>,
    iap2_tunnel: &SharedIap2Tunnel,
    progress: &SharedProgress,
    cancellation: &CancellationToken,
    accept_poll: Duration,
    io_timeout: Duration,
    media: Option<MediaController>,
) -> Result<(), String> {
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !bind_address.accepts_peer(peer) {
                    return fail_progress(
                        progress,
                        "AirPlay peer did not arrive through the selected scoped NCM interface",
                    );
                }
                bind_address
                    .verify_local(stream.local_addr().map_err(|error| {
                        format!("reading the accepted AirPlay socket address failed: {error}")
                    })?)
                    .map_err(|error| {
                        format!("accepted AirPlay socket escaped the selected interface: {error}")
                    })?;
                let SocketAddr::V6(peer) = peer else {
                    return fail_progress(progress, "AirPlay control peer is not IPv6");
                };
                if let Some(media) = media.as_ref() {
                    media.set_peer(peer)?;
                }
                stream
                    .set_nonblocking(false)
                    .map_err(|error| format!("configuring the AirPlay socket failed: {error}"))?;
                stream
                    .set_read_timeout(Some(io_timeout))
                    .map_err(|error| format!("setting AirPlay read timeout failed: {error}"))?;
                stream
                    .set_write_timeout(Some(io_timeout))
                    .map_err(|error| format!("setting AirPlay write timeout failed: {error}"))?;
                set_stage(progress, AirPlayStage::Connected);
                return run_control_connection(
                    &mut stream,
                    &mut receiver,
                    &mut ports,
                    &event_key_sender,
                    &event_command_sender,
                    iap2_tunnel,
                    progress,
                    cancellation,
                    media.as_ref(),
                )
                .inspect_err(|_| set_stage(progress, AirPlayStage::Failed));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(accept_poll);
            }
            Err(error) => {
                return fail_progress(
                    progress,
                    &format!("accepting the AirPlay control connection failed: {error}"),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_control_connection<S: MfiSigner, W: Read + Write>(
    stream: &mut W,
    receiver: &mut AirPlayReceiver<S>,
    ports: &mut SharedBoundPorts,
    event_key_sender: &mpsc::SyncSender<EventChannelKeys>,
    event_command_sender: &mpsc::SyncSender<EventWorkerCommand>,
    iap2_tunnel: &SharedIap2Tunnel,
    progress: &SharedProgress,
    cancellation: &CancellationToken,
    media: Option<&MediaController>,
) -> Result<(), String> {
    let mut codec = RtspCodec::new(RtspLimits::default())
        .map_err(|error| format!("initializing the RTSP parser failed: {error}"))?;
    let mut cipher: Option<ControlCipher> = None;
    let mut input = [0_u8; 16 * 1024];
    let mut control_events = ControlEventDecoder::default();
    let mut screen_stream_ready = false;
    let mut recording_requested = false;
    let mut data_channels =
        DataChannels::new(event_command_sender.clone(), Arc::clone(iap2_tunnel));
    let mut control_prefix = Vec::with_capacity(32);
    let mut peer_heard = Instant::now();
    let mut keep_alive_armed = false;
    let connected_at = Instant::now();
    let mut recording_reached = false;
    let mut first_video = None;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if ports.drain_keep_alive() {
            keep_alive_armed = true;
            peer_heard = Instant::now();
        }
        if keep_alive_armed && peer_heard.elapsed() >= PEER_SILENCE_TIMEOUT {
            return Err("the AirPlay peer stopped sending keep-alives".to_owned());
        }
        if !recording_reached && connected_at.elapsed() >= CONTROL_SETUP_TIMEOUT {
            tracing::warn!(
                "CarPlay did not start streaming after connecting; restarting the session"
            );
            return Err(format!(
                "CarPlay did not start streaming within {CONTROL_SETUP_TIMEOUT:?} of connecting"
            ));
        }
        if let Some(media) = media {
            check_first_screen_frame(&mut first_video, media, event_command_sender)?;
        }
        let read_buffer = if cipher.is_some() {
            &mut input[..]
        } else {
            &mut input[..1]
        };
        let length = match stream.read(read_buffer) {
            Ok(0) => return Err("the AirPlay control peer closed the connection".to_owned()),
            Ok(length) => {
                peer_heard = Instant::now();
                length
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(format!("reading AirPlay control data failed: {error}")),
        };
        let decrypted;
        let plaintext = match cipher.as_mut() {
            Some(cipher) => {
                decrypted = cipher
                    .decrypt(&read_buffer[..length])
                    .map_err(|error| format!("decrypting AirPlay control data failed: {error}"))?;
                decrypted.as_slice()
            }
            None => &read_buffer[..length],
        };
        if plaintext.is_empty() {
            continue;
        }
        if control_prefix.len() < 32 {
            let remaining = 32 - control_prefix.len();
            control_prefix.extend_from_slice(&plaintext[..plaintext.len().min(remaining)]);
        }
        let requests = codec.push(plaintext).map_err(|error| {
            format!(
                "decoding AirPlay RTSP data failed: {error}; encrypted={}; prefix={}",
                cipher.is_some(),
                hex::encode(&control_prefix)
            )
        })?;
        for request in requests {
            let transport = if cipher.is_some() {
                ControlTransport::Encrypted
            } else {
                ControlTransport::Cleartext
            };
            if let Some(media) = media {
                receiver
                    .set_audio_feedback(media.audio_feedback()?)
                    .map_err(|error| {
                        format!("publishing CarPlay audio feedback failed: {error}")
                    })?;
            }
            let output = receiver
                .handle(&request, transport, ports)
                .map_err(|error| format!("handling AirPlay RTSP request failed: {error}"))?;
            keep_alive_armed |= output.events.contains(&ReceiverEvent::KeepAlive);
            let rejection = output.events.iter().find_map(|event| match event {
                ReceiverEvent::Reset { reason, .. } => Some(reason.as_str()),
                _ => None,
            });
            if let Some(reason) = rejection {
                tracing::warn!(
                    method = %request.method,
                    path = %request.target.split('?').next().unwrap_or("[invalid]"),
                    cseq = request.headers.get("cseq").map(String::as_str),
                    reason,
                    "AirPlay receiver rejected a request"
                );
            }
            tracing::debug!(
                method = %request.method,
                path = %request.target.split('?').next().unwrap_or("[invalid]"),
                encrypted = cipher.is_some(),
                status = output.response.status,
                event_count = output.events.len(),
                content_type = request.headers.get("content-type").map(String::as_str),
                cseq = request.headers.get("cseq").map(String::as_str),
                body_length = request.body.len(),
                pairing_failure = receiver.pair_verify_failure_reason(),
                rejection,
                "handled AirPlay RTSP request"
            );
            prepare_media_output(
                receiver,
                ports,
                &output.events,
                media,
                &mut control_events,
                &mut screen_stream_ready,
                &mut recording_requested,
                &mut data_channels,
                progress,
            )?;
            if !recording_reached
                && progress
                    .lock()
                    .is_ok_and(|progress| progress.stage == AirPlayStage::Recording)
            {
                recording_reached = true;
                first_video = media.map(|media| {
                    (
                        FirstVideoWatchdog::new(Instant::now()),
                        media.screen_frames_received(),
                    )
                });
            }
            if output
                .events
                .iter()
                .any(|event| matches!(event, ReceiverEvent::RecordingStarted))
            {
                set_current_event_night_mode(event_command_sender, Duration::from_secs(1))?;
            }
            send_output(
                stream,
                &request,
                output,
                &mut cipher,
                event_key_sender,
                progress,
            )?;
            if matches!(receiver.state(), ReceiverState::Teardown) {
                return Ok(());
            }
        }
    }
}

fn check_first_screen_frame(
    first_video: &mut Option<(FirstVideoWatchdog, u64)>,
    media: &MediaController,
    event_command_sender: &mpsc::SyncSender<EventWorkerCommand>,
) -> Result<(), String> {
    if first_video
        .as_ref()
        .is_some_and(|(_, baseline)| media.screen_frames_received() > *baseline)
    {
        *first_video = None;
    }
    let Some((watchdog, _)) = first_video.as_mut() else {
        return Ok(());
    };
    match watchdog.check(Instant::now()) {
        FirstVideoAction::Wait => Ok(()),
        FirstVideoAction::RequestKeyFrame => {
            tracing::warn!(
                "CarPlay has sent no screen video since recording started; requesting a keyframe"
            );
            match event_command_sender.try_send(EventWorkerCommand::ForceKeyFrame) {
                Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    Err("queueing CarPlay keyframe request failed: event worker stopped".to_owned())
                }
            }
        }
        FirstVideoAction::Expired => {
            tracing::warn!(
                "CarPlay sent no screen video after recording started; restarting the session"
            );
            Err(format!(
                "CarPlay sent no screen video within {:?} of recording",
                FirstVideoWatchdog::TIMEOUT
            ))
        }
    }
}

fn send_output<W: Write>(
    stream: &mut W,
    request: &RtspRequest,
    output: ReceiverOutput,
    cipher: &mut Option<ControlCipher>,
    event_key_sender: &mpsc::SyncSender<EventChannelKeys>,
    progress: &SharedProgress,
) -> Result<(), String> {
    let mut encoded = encode_response(request, &output.response)
        .map_err(|error| format!("encoding AirPlay RTSP response failed: {error}"))?;
    if let Some(active) = cipher.as_mut() {
        encoded = active
            .encrypt(&encoded)
            .map_err(|error| format!("encrypting AirPlay RTSP response failed: {error}"))?;
    }
    stream
        .write_all(&encoded)
        .map_err(|error| format!("writing AirPlay RTSP response failed: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flushing AirPlay RTSP response failed: {error}"))?;
    apply_events(progress, &output.events);
    if let Some(ReceiverEncryptionKeys { control, event }) = output.control_keys {
        if cipher.is_some()
            || !output
                .events
                .iter()
                .any(|event| matches!(event, ReceiverEvent::ControlEncryptionReady { .. }))
        {
            return Err("receiver attempted an invalid control-encryption transition".to_owned());
        }
        event_key_sender
            .try_send(event)
            .map_err(|_| "AirPlay event keys could not be installed exactly once".to_owned())?;
        *cipher = Some(
            ControlCipher::new(control)
                .map_err(|error| format!("installing AirPlay control keys failed: {error}"))?,
        );
        set_stage(progress, AirPlayStage::EncryptedControl);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_media_output<S: MfiSigner>(
    receiver: &AirPlayReceiver<S>,
    ports: &SharedBoundPorts,
    events: &[ReceiverEvent],
    media: Option<&MediaController>,
    control_decoder: &mut ControlEventDecoder,
    screen_stream_ready: &mut bool,
    recording_requested: &mut bool,
    data_channels: &mut DataChannels,
    progress: &SharedProgress,
) -> Result<(), String> {
    for event in events {
        match event {
            ReceiverEvent::StreamNegotiated {
                stream: StreamDescriptor::DataChannel(channel),
                ..
            } => {
                let key = StreamDescriptor::DataChannel(channel.clone()).key();
                info!(
                    client_type = ?channel.client_type_uuid,
                    channel_id = ?channel.channel_id,
                    "AirPlay data channel negotiated"
                );
                let listener = ports
                    .take_data_channel_listener(key)
                    .map_err(|error| format!("claiming the data channel socket failed: {error}"))?
                    .ok_or_else(|| "the data channel socket was already claimed".to_owned())?;
                let read_key = receiver
                    .active_data_stream_key(key, DataStreamDirection::Output)
                    .map_err(|error| {
                        format!("deriving the data channel read key failed: {error}")
                    })?;
                let write_key = receiver
                    .active_data_stream_key(key, DataStreamDirection::Input)
                    .map_err(|error| {
                        format!("deriving the data channel write key failed: {error}")
                    })?;
                let keys = ControlKeys::new(*read_key, *write_key)
                    .map_err(|error| format!("installing data channel keys failed: {error}"))?;
                data_channels.open(key, listener, keys, channel.client_type_uuid.clone())?;
            }
            ReceiverEvent::SessionNegotiated {
                timing,
                ports: negotiated_ports,
            } => {
                tracing::debug!(
                    timing_port = negotiated_ports.timing_port.get(),
                    event_port = negotiated_ports.event_port.get(),
                    keep_alive_port = negotiated_ports.keep_alive_port.map(NonZeroU16::get),
                    "negotiated AirPlay session ports"
                );
                let media = require_media(media)?;
                let socket = ports
                    .take_timing_socket()
                    .map_err(|error| format!("claiming timing socket failed: {error}"))?
                    .ok_or_else(|| "timing socket was already claimed".to_owned())?;
                media.start_timing(timing.clone(), socket)?;
            }
            ReceiverEvent::StreamNegotiated { stream, .. } => {
                match stream {
                    StreamDescriptor::Audio(audio) => info!(
                        stream_type = audio.stream_type(),
                        audio_type = %audio.audio_type,
                        audio_format = format_args!("{:#x}", audio.audio_format),
                        audio_latency_ms = ?audio.audio_latency_ms,
                        frames_per_packet = ?audio.frames_per_packet,
                        microphone = audio.microphone.is_some(),
                        "CarPlay audio stream negotiated"
                    ),
                    StreamDescriptor::Screen(screen) => info!(
                        stream_type = screen.stream_type(),
                        "CarPlay screen stream negotiated"
                    ),
                    StreamDescriptor::DataChannel(_) => {
                        unreachable!("data channels are matched before media streams")
                    }
                }
                let media = require_media(media)?;
                let sockets = ports
                    .take_media_sockets(stream.key())
                    .map_err(|error| format!("claiming media sockets failed: {error}"))?
                    .ok_or_else(|| "negotiated media sockets were already claimed".to_owned())?;
                let output_key = receiver
                    .active_data_stream_key(stream.key(), DataStreamDirection::Output)
                    .map_err(|error| format!("deriving media output key failed: {error}"))?;
                let input_key = matches!(
                    stream,
                    StreamDescriptor::Audio(audio) if audio.microphone.is_some()
                )
                .then(|| {
                    receiver
                        .active_data_stream_key(stream.key(), DataStreamDirection::Input)
                        .map_err(|error| format!("deriving microphone input key failed: {error}"))
                })
                .transpose()?;
                media.start_stream(stream.clone(), sockets, output_key, input_key)?;
                *screen_stream_ready |= matches!(stream, StreamDescriptor::Screen(_));
            }
            ReceiverEvent::RecordingStarted => {
                *recording_requested = true;
                info!("CarPlay RECORD received");
            }
            ReceiverEvent::CommandReceived(body) => {
                let decoded = match control_decoder.decode(body) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            body_length = body.len(),
                            "ignoring invalid authenticated CarPlay command"
                        );
                        continue;
                    }
                };
                match decoded {
                    Some(ControlEvent::DuckAudio {
                        duration_ms,
                        volume_db,
                        linear_gain,
                    }) => {
                        info!(
                            volume_db,
                            linear_gain, duration_ms, "CarPlay audio duck requested"
                        );
                        require_media(media)?.set_ducking(true, Some(linear_gain), duration_ms)?;
                    }
                    Some(ControlEvent::UnduckAudio { duration_ms }) => {
                        info!(duration_ms, "CarPlay audio unduck requested");
                        require_media(media)?.set_ducking(false, None, duration_ms)?;
                    }
                    Some(ControlEvent::SiriSpeechChanged { mode, active }) => {
                        info!(?mode, active, "CarPlay Siri speech mode changed");
                    }
                    Some(ControlEvent::DisableBluetooth) => {
                        info!("the iPhone handed CarPlay off from Bluetooth to Wi-Fi");
                        if let Ok(mut current) = progress.lock() {
                            current.bluetooth_handoff_requested = true;
                        }
                    }
                    Some(ControlEvent::HostUiRequested) | None => {}
                }
            }
            ReceiverEvent::StreamsTornDown(streams) => {
                data_channels.close(streams);
                require_media(media)?.teardown(streams.clone())?;
                ports
                    .release_media_sockets(streams)
                    .map_err(|error| format!("replenishing media socket pool failed: {error}"))?;
            }
            ReceiverEvent::Reset { streams, reason } => {
                tracing::warn!(reason, "AirPlay receiver rejected the session");
                data_channels.close_all();
                require_media(media)?.teardown(streams.clone())?;
                *screen_stream_ready = false;
                *recording_requested = false;
            }
            ReceiverEvent::SessionTornDown => {
                data_channels.close_all();
                require_media(media)?.teardown(Vec::new())?;
                *screen_stream_ready = false;
                *recording_requested = false;
            }
            ReceiverEvent::Flushed(point) => {
                require_media(media)?.flush(*point)?;
            }
            ReceiverEvent::ControlEncryptionReady { .. }
            | ReceiverEvent::MfiAuthenticated
            | ReceiverEvent::InfoRequested
            | ReceiverEvent::ParametersSet(_)
            | ReceiverEvent::ParametersRequested(_)
            | ReceiverEvent::KeepAlive => {}
        }
    }
    if *recording_requested && *screen_stream_ready {
        let recording = require_media(media)?.record()?;
        if let Ok(mut current) = progress.lock() {
            current.media_recording = recording;
            if recording {
                current.stage = AirPlayStage::Recording;
            }
        }
        if recording {
            *recording_requested = false;
        }
    }
    Ok(())
}

fn require_media(media: Option<&MediaController>) -> Result<&MediaController, String> {
    media.ok_or_else(|| "negotiated media arrived without a preflighted backend".to_owned())
}

#[allow(clippy::too_many_arguments)]
fn run_event_listener(
    listener: TcpListener,
    bind_address: ScopedBindAddress,
    key_receiver: mpsc::Receiver<EventChannelKeys>,
    command_receiver: mpsc::Receiver<EventWorkerCommand>,
    input: Option<Box<dyn LiveInputSource>>,
    progress: &SharedProgress,
    cancellation: &CancellationToken,
    ambient_light: &AmbientLightState,
    presets: &PresetBindings,
    accept_poll: Duration,
    read_timeout: Duration,
    write_timeout: Duration,
    media: Option<MediaController>,
    #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
        prelude_media::LinuxDisplayHandle,
    >,
) -> Result<(), String> {
    let mut keys = None;
    let mut accepted = None;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if keys.is_none() {
            match key_receiver.try_recv() {
                Ok(received) => {
                    tracing::debug!("received AirPlay event-channel encryption keys");
                    keys = Some(received);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(
                        "AirPlay control closed before event-channel keys were available"
                            .to_owned(),
                    );
                }
            }
        }
        if accepted.is_none() {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if !bind_address.accepts_peer(peer) {
                        return Err(
                            "AirPlay event peer did not arrive through the selected scoped NCM interface"
                                .to_owned(),
                        );
                    }
                    bind_address
                        .verify_local(stream.local_addr().map_err(|error| {
                            format!("reading the accepted event socket address failed: {error}")
                        })?)
                        .map_err(|error| {
                            format!("accepted event socket escaped the selected interface: {error}")
                        })?;
                    tracing::debug!(peer = %peer, "accepted AirPlay event-channel peer");
                    accepted = Some(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    return Err(format!(
                        "accepting the AirPlay event connection failed: {error}"
                    ));
                }
            }
        }
        if let Some((mut stream, keys)) = take_ready_event_channel(&mut accepted, &mut keys) {
            tracing::debug!("starting authenticated AirPlay event channel");
            stream
                .set_nonblocking(false)
                .map_err(|error| format!("configuring the AirPlay event socket failed: {error}"))?;
            stream
                .set_read_timeout(Some(read_timeout.min(RECORD_EVENT_POLL_INTERVAL)))
                .map_err(|error| format!("setting the event read timeout failed: {error}"))?;
            stream
                .set_write_timeout(Some(write_timeout))
                .map_err(|error| format!("setting the event write timeout failed: {error}"))?;
            let result = run_event_connection_with_ambient(
                &mut stream,
                keys,
                &command_receiver,
                input,
                progress,
                cancellation,
                ambient_light,
                presets,
                media.as_ref(),
                #[cfg(all(target_os = "linux", feature = "linux-media"))]
                display.as_ref(),
            );
            let _ = stream.shutdown(Shutdown::Both);
            return result;
        }
        thread::sleep(accept_poll.min(RECORD_EVENT_POLL_INTERVAL));
    }
}

fn take_ready_event_channel<S>(
    accepted: &mut Option<S>,
    keys: &mut Option<EventChannelKeys>,
) -> Option<(S, EventChannelKeys)> {
    if accepted.is_none() || keys.is_none() {
        return None;
    }
    Some((
        accepted
            .take()
            .expect("event stream was checked before it was consumed"),
        keys.take()
            .expect("event keys were checked before they were consumed"),
    ))
}

fn bounded_event_read_timeout(
    control_io_timeout: Duration,
    input_poll_interval: Option<Duration>,
) -> Duration {
    input_poll_interval.map_or(control_io_timeout, |poll| control_io_timeout.min(poll))
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn run_event_connection<S: Read + Write>(
    stream: &mut S,
    keys: EventChannelKeys,
    command_receiver: &mpsc::Receiver<EventWorkerCommand>,
    input: Option<Box<dyn LiveInputSource>>,
    progress: &SharedProgress,
    cancellation: &CancellationToken,
    media: Option<&MediaController>,
) -> Result<(), String> {
    run_event_connection_with_ambient(
        stream,
        keys,
        command_receiver,
        input,
        progress,
        cancellation,
        &AmbientLightState::default(),
        &PresetBindings::default(),
        media,
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_event_connection_with_ambient<S: Read + Write>(
    stream: &mut S,
    keys: EventChannelKeys,
    command_receiver: &mpsc::Receiver<EventWorkerCommand>,
    mut input: Option<Box<dyn LiveInputSource>>,
    progress: &SharedProgress,
    cancellation: &CancellationToken,
    ambient_light: &AmbientLightState,
    presets: &PresetBindings,
    media: Option<&MediaController>,
    #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
        &prelude_media::LinuxDisplayHandle,
    >,
) -> Result<(), String> {
    let mut cipher = EventChannelCipher::new(keys)
        .map_err(|error| format!("installing AirPlay event keys failed: {error}"))?;
    let mut codec = EventChannelCodec::new(RtspLimits::default())
        .map_err(|error| format!("initializing the event RTSP parser failed: {error}"))?;
    let mut wire = InputWireSession::new();
    let mut pending_responses = BTreeSet::new();
    let mut last_timestamp = MonotonicTimestamp(0);
    let mut last_night_mode = None;
    if let Ok(mut progress) = progress.lock() {
        progress.event_channel_ready = true;
    }
    let mut read_buffer = [0_u8; 16 * 1024];
    let mut input_suppressed_last = false;
    loop {
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let input_suppressed = display.is_some_and(|display| display.host_ui_active());
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        let input_suppressed = false;
        if input_suppressed && !input_suppressed_last {
            send_modal_input_releases(
                stream,
                &mut cipher,
                &mut wire,
                &mut pending_responses,
                last_timestamp,
            )?;
        }
        input_suppressed_last = input_suppressed;
        if cancellation.is_cancelled() {
            if let Some(input) = input.as_mut() {
                let batch = input.cancel()?;
                process_input_batch(
                    stream,
                    &mut cipher,
                    &mut wire,
                    batch,
                    &mut pending_responses,
                    &mut last_timestamp,
                    progress,
                    media,
                    presets,
                    input_suppressed,
                )?;
            }
            send_forced_releases(
                stream,
                &mut cipher,
                &mut wire,
                &mut pending_responses,
                last_timestamp,
            )?;
            return Ok(());
        }
        process_event_worker_commands(
            stream,
            &mut cipher,
            &mut wire,
            &mut pending_responses,
            last_timestamp,
            command_receiver,
            &mut last_night_mode,
            ambient_light,
        )?;
        if let Some(last) = last_night_mode {
            let desired = ambient_light.night_mode();
            if desired != last {
                send_night_mode_event(
                    stream,
                    &mut cipher,
                    &mut wire,
                    &mut pending_responses,
                    desired,
                    last_timestamp,
                )?;
                last_night_mode = Some(desired);
                tracing::debug!(
                    enabled = desired,
                    "updated CarPlay night mode from ambient light"
                );
            }
        }
        match stream.read(&mut read_buffer) {
            Ok(0) => return Err("the AirPlay event peer closed the connection".to_owned()),
            Ok(length) => {
                let plaintext = cipher
                    .decrypt(&read_buffer[..length])
                    .map_err(|error| format!("decrypting AirPlay event data failed: {error}"))?;
                for message in codec
                    .push(&plaintext)
                    .map_err(|error| format!("decoding AirPlay event RTSP failed: {error}"))?
                {
                    handle_event_message(stream, &mut cipher, message, &mut pending_responses)?;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(format!("reading AirPlay event data failed: {error}")),
        }
        if let Some(input) = input.as_mut() {
            let batch = input.poll()?;
            let terminal = batch.terminal;
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            let input_suppressed = display.is_some_and(|display| display.host_ui_active());
            #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
            let input_suppressed = false;
            if input_suppressed && !input_suppressed_last {
                send_modal_input_releases(
                    stream,
                    &mut cipher,
                    &mut wire,
                    &mut pending_responses,
                    last_timestamp,
                )?;
            }
            input_suppressed_last = input_suppressed;
            process_input_batch(
                stream,
                &mut cipher,
                &mut wire,
                batch,
                &mut pending_responses,
                &mut last_timestamp,
                progress,
                media,
                presets,
                input_suppressed,
            )?;
            if let Some(terminal) = terminal {
                send_forced_releases(
                    stream,
                    &mut cipher,
                    &mut wire,
                    &mut pending_responses,
                    last_timestamp,
                )?;
                if let Ok(mut progress) = progress.lock() {
                    progress.input_ready = false;
                }
                return match terminal {
                    InputTerminal::Cancelled if cancellation.is_cancelled() => Ok(()),
                    InputTerminal::Cancelled | InputTerminal::DeviceUnavailable => {
                        Err("the Superbird input backend became terminal during CarPlay".to_owned())
                    }
                };
            }
        }
    }
}

fn send_modal_input_releases<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    wire: &mut InputWireSession,
    pending_responses: &mut BTreeSet<u32>,
    timestamp: MonotonicTimestamp,
) -> Result<(), String> {
    let releases = wire.release_all(timestamp).map_err(|error| {
        format!("encoding CarPlay host-settings input releases failed: {error}")
    })?;
    for release in releases {
        send_airplay_event(stream, cipher, release, pending_responses)?;
    }
    Ok(())
}

fn handle_event_message<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    message: EventChannelMessage,
    pending_responses: &mut BTreeSet<u32>,
) -> Result<(), String> {
    match message {
        EventChannelMessage::Request(request) => {
            tracing::debug!(
                method = %request.method,
                path = %request.target,
                cseq = request.headers.get("cseq").map(String::as_str),
                body_length = request.body.len(),
                "handled authenticated AirPlay event-channel request"
            );
            let encoded = encode_response(&request, &RtspResponse::new(200, "OK"))
                .map_err(|error| format!("encoding an event-channel response failed: {error}"))?;
            write_event_plaintext(stream, cipher, &encoded)
        }
        EventChannelMessage::Response(response) => {
            let sequence = match response.headers.get("cseq") {
                Some(value) => value
                    .parse::<u32>()
                    .map_err(|_| "AirPlay event response has an invalid CSeq".to_owned())?,
                None if !pending_responses.is_empty() => *pending_responses
                    .first()
                    .expect("a pending response was checked before access"),
                None => {
                    return Err("AirPlay event response arrived with no pending request".to_owned());
                }
            };
            if !pending_responses.remove(&sequence) {
                return Err(
                    "AirPlay event response was unexpected, duplicated, or replayed".to_owned(),
                );
            }
            if response.status != 200 {
                return Err(format!(
                    "AirPlay event command was rejected with status {}",
                    response.status
                ));
            }
            tracing::debug!(
                cseq = sequence,
                status = response.status,
                body_length = response.body.len(),
                "received authenticated AirPlay event-channel response"
            );
            Ok(())
        }
    }
}

fn set_current_event_night_mode(
    sender: &mpsc::SyncSender<EventWorkerCommand>,
    timeout: Duration,
) -> Result<(), String> {
    set_event_night_mode_request(sender, None, timeout)
}

fn set_event_night_mode_request(
    sender: &mpsc::SyncSender<EventWorkerCommand>,
    enabled: Option<bool>,
    timeout: Duration,
) -> Result<(), String> {
    let (completed, completion) = mpsc::sync_channel(1);
    sender
        .try_send(EventWorkerCommand::SetNightMode { enabled, completed })
        .map_err(|error| format!("queueing the initial CarPlay vehicle state failed: {error}"))?;
    completion
        .recv_timeout(timeout)
        .map_err(|error| format!("sending the initial CarPlay vehicle state timed out: {error}"))?
}

#[allow(clippy::too_many_arguments)]
fn process_event_worker_commands<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    wire: &mut InputWireSession,
    pending_responses: &mut BTreeSet<u32>,
    timestamp: MonotonicTimestamp,
    receiver: &mpsc::Receiver<EventWorkerCommand>,
    last_night_mode: &mut Option<bool>,
    ambient_light: &AmbientLightState,
) -> Result<(), String> {
    loop {
        let command = match receiver.try_recv() {
            Ok(command) => command,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return Ok(()),
        };
        match command {
            EventWorkerCommand::SetNightMode { enabled, completed } => {
                let enabled = enabled.unwrap_or_else(|| ambient_light.night_mode());
                let result = send_night_mode_event(
                    stream,
                    cipher,
                    wire,
                    pending_responses,
                    enabled,
                    timestamp,
                );
                let _ = completed.send(result.clone());
                result?;
                *last_night_mode = Some(enabled);
                tracing::debug!(enabled, "sent CarPlay night-mode vehicle state");
            }
            EventWorkerCommand::ForceKeyFrame => {
                let event =
                    wire.force_key_frame(MAIN_DISPLAY_UUID, timestamp)
                        .map_err(|error| {
                            format!("encoding CarPlay keyframe request failed: {error}")
                        })?;
                send_airplay_event(stream, cipher, event, pending_responses)?;
                tracing::info!(
                    display_uuid = MAIN_DISPLAY_UUID,
                    "requested clean CarPlay keyframe"
                );
            }
            EventWorkerCommand::IapSendMessage(data) => {
                let event = wire
                    .iap_message(&data, timestamp)
                    .map_err(|error| format!("encoding an iAP2 tunnel message failed: {error}"))?;
                send_airplay_event(stream, cipher, event, pending_responses)?;
            }
        }
    }
}

fn send_night_mode_event<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    wire: &mut InputWireSession,
    pending_responses: &mut BTreeSet<u32>,
    enabled: bool,
    timestamp: MonotonicTimestamp,
) -> Result<(), String> {
    let event = wire
        .set_night_mode(enabled, timestamp)
        .map_err(|error| format!("encoding CarPlay night mode failed: {error}"))?;
    send_airplay_event(stream, cipher, event, pending_responses)
}

#[allow(clippy::too_many_arguments)]
fn process_input_batch<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    wire: &mut InputWireSession,
    batch: LiveInputBatch,
    pending_responses: &mut BTreeSet<u32>,
    last_timestamp: &mut MonotonicTimestamp,
    progress: &SharedProgress,
    media: Option<&MediaController>,
    presets: &PresetBindings,
    input_suppressed: bool,
) -> Result<(), String> {
    for input in batch.events {
        if input_suppressed {
            continue;
        }
        *last_timestamp = MonotonicTimestamp(input.timestamp.0.max(last_timestamp.0));
        match encode_platform_input(wire, input, presets)? {
            InputDisposition::CarPlay { first, second } => {
                let event_count = 1 + usize::from(second.is_some());
                ensure_event_response_capacity(pending_responses, event_count)?;
                for event in std::iter::once(first).chain(second) {
                    send_airplay_event(stream, cipher, event, pending_responses)?;
                    if let Ok(mut progress) = progress.lock() {
                        progress.input_event_count = progress.input_event_count.saturating_add(1);
                    }
                }
            }
            InputDisposition::LocalMute => {
                media
                    .ok_or_else(|| {
                        "local mute arrived while the USB audio mixer was unavailable".to_owned()
                    })?
                    .toggle_mute()?;
                if let Ok(mut progress) = progress.lock() {
                    progress.local_event_count = progress.local_event_count.saturating_add(1);
                }
            }
            InputDisposition::Ignore => {}
            InputDisposition::Reset => {}
        }
    }
    Ok(())
}

enum InputDisposition {
    CarPlay {
        first: AirPlayEvent,
        second: Option<AirPlayEvent>,
    },
    LocalMute,
    Ignore,
    Reset,
}

#[derive(Debug, Clone, Copy)]
enum PresetInput {
    Media(MediaButton),
    Knob(KnobButton),
    Telephony(TelephonyButton),
    Siri,
}

impl PresetInput {
    fn encode(
        self,
        wire: &mut InputWireSession,
        pressed: bool,
        timestamp: MonotonicTimestamp,
    ) -> prelude_control::Result<AirPlayEvent> {
        match self {
            Self::Media(button) => wire.media_button(button, pressed, timestamp),
            Self::Knob(button) => wire.knob_button(button, pressed, timestamp),
            Self::Telephony(button) => wire.telephony_button(button, pressed, timestamp),
            Self::Siri => wire.siri_button(pressed, timestamp),
        }
    }
}

fn encode_platform_input(
    wire: &mut InputWireSession,
    input: PlatformInputEvent,
    presets: &PresetBindings,
) -> Result<InputDisposition, String> {
    let timestamp = MonotonicTimestamp(input.timestamp.0);
    let event = match input.kind {
        PlatformInputEventKind::Touch {
            contact_id,
            phase,
            x,
            y,
        } => wire.touch(
            contact_id,
            match phase {
                TouchInputPhase::Down => TouchAction::Down,
                TouchInputPhase::Move => TouchAction::Move,
                TouchInputPhase::Up => TouchAction::Up,
            },
            x,
            y,
            timestamp,
        ),
        PlatformInputEventKind::KnobButton { button, pressed } => wire.knob_button(
            match button {
                KnobInputButton::Back => KnobButton::Back,
                KnobInputButton::Select => KnobButton::Select,
            },
            pressed,
            timestamp,
        ),
        PlatformInputEventKind::Rotary { direction, steps } => wire.rotate(
            match direction {
                RotaryDirection::Clockwise => i32::from(steps),
                RotaryDirection::CounterClockwise => -i32::from(steps),
            },
            timestamp,
        ),
        PlatformInputEventKind::SiriButton { pressed } => wire.siri_button(pressed, timestamp),
        PlatformInputEventKind::VoiceAssistantCancel => {
            return Ok(InputDisposition::CarPlay {
                first: wire.cancel_siri(timestamp).map_err(|error| {
                    format!("encoding CarPlay voice-assistant cancellation failed: {error}")
                })?,
                second: None,
            });
        }
        PlatformInputEventKind::SettingsButton => return Ok(InputDisposition::Ignore),
        PlatformInputEventKind::Local(LocalInputAction::Mute) => {
            return Ok(InputDisposition::LocalMute);
        }
        PlatformInputEventKind::Local(LocalInputAction::Preset(preset)) => {
            let control = match presets.action(preset) {
                PresetAction::None => return Ok(InputDisposition::Ignore),
                PresetAction::Mute => return Ok(InputDisposition::LocalMute),
                PresetAction::PreviousTrack => PresetInput::Media(MediaButton::Previous),
                PresetAction::PlayPause => PresetInput::Media(MediaButton::PlayPause),
                PresetAction::NextTrack => PresetInput::Media(MediaButton::Next),
                PresetAction::Home => PresetInput::Knob(KnobButton::Home),
                PresetAction::Back => PresetInput::Knob(KnobButton::Back),
                PresetAction::VoiceAssistant => PresetInput::Siri,
                PresetAction::AnswerCall => PresetInput::Telephony(TelephonyButton::HookSwitch),
                PresetAction::EndCall => PresetInput::Telephony(TelephonyButton::Drop),
            };
            let mut staged = wire.clone();
            let pressed = control
                .encode(&mut staged, true, timestamp)
                .map_err(|error| format!("encoding Superbird input for CarPlay failed: {error}"))?;
            let released = control
                .encode(&mut staged, false, timestamp)
                .map_err(|error| format!("encoding Superbird input for CarPlay failed: {error}"))?;
            *wire = staged;
            return Ok(InputDisposition::CarPlay {
                first: pressed,
                second: Some(released),
            });
        }
        PlatformInputEventKind::Reset { .. } => return Ok(InputDisposition::Reset),
    }
    .map_err(|error| format!("encoding Superbird input for CarPlay failed: {error}"))?;
    Ok(InputDisposition::CarPlay {
        first: event,
        second: None,
    })
}

fn send_airplay_event<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    event: AirPlayEvent,
    pending_responses: &mut BTreeSet<u32>,
) -> Result<(), String> {
    ensure_event_response_capacity(pending_responses, 1)?;
    if !pending_responses.insert(event.sequence) {
        return Err("AirPlay event CSeq was reused".to_owned());
    }
    write_event_plaintext(stream, cipher, &event.message)
}

fn ensure_event_response_capacity(
    pending_responses: &BTreeSet<u32>,
    additional: usize,
) -> Result<(), String> {
    if additional > MAXIMUM_PENDING_EVENT_RESPONSES.saturating_sub(pending_responses.len()) {
        return Err("AirPlay event response window is exhausted".to_owned());
    }
    Ok(())
}

fn send_forced_releases<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    wire: &mut InputWireSession,
    pending_responses: &mut BTreeSet<u32>,
    timestamp: MonotonicTimestamp,
) -> Result<(), String> {
    if wire.is_torn_down() {
        return Ok(());
    }
    for release in wire
        .teardown(timestamp)
        .map_err(|error| format!("encoding forced CarPlay input releases failed: {error}"))?
    {
        send_airplay_event(stream, cipher, release, pending_responses)?;
    }
    Ok(())
}

fn write_event_plaintext<W: Write>(
    stream: &mut W,
    cipher: &mut EventChannelCipher,
    plaintext: &[u8],
) -> Result<(), String> {
    let encrypted = cipher
        .encrypt(plaintext)
        .map_err(|error| format!("encrypting AirPlay event data failed: {error}"))?;
    stream
        .write_all(&encrypted)
        .map_err(|error| format!("writing AirPlay event data failed: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flushing AirPlay event data failed: {error}"))
}

fn record_failure(failure: &Arc<Mutex<Option<String>>>, detail: &str) {
    if let Ok(mut failure) = failure.lock() {
        if failure.is_none() {
            *failure = Some(detail.to_owned());
        }
    }
}

fn apply_events(progress: &SharedProgress, events: &[ReceiverEvent]) {
    if let Ok(mut progress) = progress.lock() {
        for event in events {
            progress.event_count = progress.event_count.saturating_add(1);
            match event {
                ReceiverEvent::ControlEncryptionReady { .. } => {
                    progress.stage = AirPlayStage::EncryptedControl;
                }
                ReceiverEvent::MfiAuthenticated => {
                    progress.stage = AirPlayStage::MfiAuthenticated;
                }
                ReceiverEvent::SessionNegotiated { .. } => {
                    progress.stage = AirPlayStage::SessionConfigured;
                }
                ReceiverEvent::StreamNegotiated { .. } => {
                    progress.negotiated_streams = progress.negotiated_streams.saturating_add(1);
                    if progress.stage != AirPlayStage::Recording {
                        progress.stage = AirPlayStage::StreamsConfigured;
                    }
                }
                ReceiverEvent::RecordingStarted => {
                    progress.stage = AirPlayStage::Recording;
                }
                ReceiverEvent::StreamsTornDown(streams) => {
                    progress.negotiated_streams =
                        progress.negotiated_streams.saturating_sub(streams.len());
                }
                ReceiverEvent::SessionTornDown | ReceiverEvent::Reset { .. } => {
                    progress.negotiated_streams = 0;
                    progress.media_recording = false;
                    progress.stage = AirPlayStage::Failed;
                }
                ReceiverEvent::InfoRequested
                | ReceiverEvent::CommandReceived(_)
                | ReceiverEvent::ParametersSet(_)
                | ReceiverEvent::ParametersRequested(_)
                | ReceiverEvent::Flushed(_)
                | ReceiverEvent::KeepAlive => {}
            }
        }
    }
}

fn set_stage(progress: &SharedProgress, stage: AirPlayStage) {
    if let Ok(mut progress) = progress.lock() {
        progress.stage = stage;
    }
}

fn fail_progress<T>(progress: &SharedProgress, detail: &str) -> Result<T, String> {
    set_stage(progress, AirPlayStage::Failed);
    Err(detail.to_owned())
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn receiver_start_metadata(
    service_port: u16,
    host_mac: [u8; 6],
    identity: &AccessoryIdentity,
) -> ReceiverStartMetadata {
    ReceiverStartMetadata {
        service_port,
        device_identifier: format_mac(host_mac),
        public_key: identity.pairing_id().to_owned(),
        source_version: CARPLAY_SOURCE_VERSION.to_owned(),
    }
}

fn pairing_directory(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("airplay")
}

fn read_setup_code(path: &Path) -> Result<String, String> {
    let mut encoded = Zeroizing::new(read_bounded_regular_file(
        path,
        MAXIMUM_SETUP_CODE_FILE_BYTES,
        true,
    )?);
    if encoded.last() == Some(&b'\n') {
        encoded.pop();
        if encoded.last() == Some(&b'\r') {
            encoded.pop();
        }
    }
    if encoded.is_empty()
        || encoded.len() > 32
        || !encoded
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(
            "AirPlay setup-code file must contain 1..=32 ASCII digits or hyphens".to_owned(),
        );
    }
    String::from_utf8(encoded.to_vec())
        .map_err(|_| "AirPlay setup-code file is not valid ASCII".to_owned())
}

fn build_info_payload(
    config: &AirPlayConfig,
    identity: &Iap2Config,
    host_mac: [u8; 6],
    input_declared: bool,
    media: ReceiverMediaClaims,
) -> Result<InfoPayload, String> {
    if !media.screen {
        return Err(
            "AirPlay /info cannot claim a display without a preflighted H.264 backend".to_owned(),
        );
    }
    if media.microphone && !media.audio {
        return Err("AirPlay microphone capability requires the USB audio mixer".to_owned());
    }
    let name = required_identity(&identity.accessory_name, "accessory name")?;
    let model = required_identity(&identity.model_identifier, "model identifier")?;
    let manufacturer = required_identity(&identity.manufacturer, "manufacturer")?;
    let mut info = Dictionary::new();
    info.insert(
        "sourceVersion".to_owned(),
        Value::String(CARPLAY_SOURCE_VERSION.to_owned()),
    );
    info.insert(
        "features".to_owned(),
        Value::from(if media.audio {
            CARPLAY_FEATURES
        } else {
            CARPLAY_FEATURES_NO_AUDIO
        }),
    );
    info.insert("statusFlags".to_owned(), Value::from(4_u64));
    info.insert("model".to_owned(), Value::String(model.to_owned()));
    info.insert(
        "manufacturer".to_owned(),
        Value::String(manufacturer.to_owned()),
    );
    info.insert("deviceID".to_owned(), Value::String(format_mac(host_mac)));
    info.insert("name".to_owned(), Value::String(name.to_owned()));
    info.insert(
        "rightHandDrive".to_owned(),
        Value::Boolean(config.dock.right_hand_drive()),
    );
    info.insert("keepAliveLowPower".to_owned(), Value::Boolean(true));
    info.insert("keepAliveSendStatsAsBody".to_owned(), Value::Boolean(false));
    info.insert(
        "modes".to_owned(),
        Value::Dictionary(info_modes(media.audio)),
    );
    info.insert(
        "extendedFeatures".to_owned(),
        Value::Array(vec![
            Value::String("vocoderInfo".to_owned()),
            Value::String("enhancedRequestCarUI".to_owned()),
        ]),
    );
    info.insert("oemIconVisible".to_owned(), Value::Boolean(false));
    info.insert(
        "displays".to_owned(),
        Value::Array(vec![Value::Dictionary(main_display(config))]),
    );
    if input_declared {
        info.insert(
            "hidDevices".to_owned(),
            Value::Array(
                prelude_control::carplay_hid_devices(MAIN_DISPLAY_UUID)
                    .into_iter()
                    .map(|device| Value::Dictionary(hid_device(device)))
                    .collect(),
            ),
        );
    }
    if media.audio {
        info.insert(
            "audioLatencies".to_owned(),
            Value::Array(info_audio_latencies()),
        );
        info.insert(
            "audioFormats".to_owned(),
            Value::Array(info_audio_formats(media.microphone)),
        );
    }
    InfoPayload::from_dictionary(info, PlistLimits::default())
        .map_err(|error| format!("building the deterministic AirPlay /info plist failed: {error}"))
}

fn info_audio_latencies() -> Vec<Value> {
    let input_micros = u64::from(
        prelude_media::MICROPHONE_CAPTURE_PERIOD_MICROS
            + prelude_media::DEFAULT_MICROPHONE_FRAME_MICROS,
    );
    let output_micros =
        u64::from(prelude_media::AudioLatencyConfig::default().output_latency_micros());
    let latency = |stream_type: u64, audio_type: Option<&str>| {
        let mut value = Dictionary::new();
        value.insert("type".to_owned(), Value::from(stream_type));
        value.insert("inputLatencyMicros".to_owned(), Value::from(input_micros));
        value.insert("outputLatencyMicros".to_owned(), Value::from(output_micros));
        if let Some(audio_type) = audio_type {
            value.insert("audioType".to_owned(), Value::String(audio_type.to_owned()));
        }
        Value::Dictionary(value)
    };
    vec![
        latency(100, None),
        latency(100, Some("default")),
        latency(100, Some("media")),
        latency(100, Some("telephony")),
        latency(100, Some("speechRecognition")),
        latency(100, Some("alert")),
        latency(101, None),
        latency(101, Some("default")),
        latency(102, Some("default")),
    ]
}

fn info_audio_formats(microphone: bool) -> Vec<Value> {
    let format = |stream_type: u64, audio_type: &str, output: u64, input: Option<u64>| {
        let mut value = Dictionary::new();
        value.insert("type".to_owned(), Value::from(stream_type));
        value.insert("audioType".to_owned(), Value::String(audio_type.to_owned()));
        value.insert("audioOutputFormats".to_owned(), Value::from(output));
        if let Some(input) = input.filter(|_| microphone) {
            value.insert("audioInputFormats".to_owned(), Value::from(input));
        }
        Value::Dictionary(value)
    };
    let pcm = PCM_VOICE_FORMATS | PCM_MEDIA_48K_FORMATS;
    let pcm_mono = PCM_MONO_48K_FORMATS;
    let opus = OPUS_MONO_FORMATS;
    vec![
        format(100, "compatibility", pcm, Some(pcm_mono)),
        format(101, "compatibility", pcm, None),
        format(100, "default", pcm | opus, Some(pcm_mono | opus)),
        format(100, "alert", pcm | opus, None),
        format(100, "media", pcm, None),
        format(100, "telephony", pcm_mono | opus, Some(pcm_mono | opus)),
        format(
            100,
            "speechRecognition",
            pcm_mono | opus,
            Some(pcm_mono | opus),
        ),
        format(101, "default", pcm | opus, None),
        format(102, "media", AAC_LC_48K_STEREO_FORMAT, None),
    ]
}

fn required_identity<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str, String> {
    value
        .as_deref()
        .filter(|value| !value.is_empty() && !value.as_bytes().contains(&0))
        .ok_or_else(|| format!("AirPlay /info requires a factual {field}"))
}

fn info_modes(audio: bool) -> Dictionary {
    let resource = |resource_id| {
        let mut resource = Dictionary::new();
        resource.insert("resourceID".to_owned(), Value::from(resource_id));
        resource.insert("transferType".to_owned(), Value::from(1_u64));
        resource.insert("transferPriority".to_owned(), Value::from(100_u64));
        resource.insert("takeConstraint".to_owned(), Value::from(100_u64));
        resource.insert("borrowConstraint".to_owned(), Value::from(100_u64));
        resource.insert("unborrowConstraint".to_owned(), Value::from(100_u64));
        Value::Dictionary(resource)
    };
    let mut call = Dictionary::new();
    call.insert("appStateID".to_owned(), Value::from(2_u64));
    call.insert("state".to_owned(), Value::Boolean(false));
    let mut speech = Dictionary::new();
    speech.insert("appStateID".to_owned(), Value::from(1_u64));
    speech.insert("speechMode".to_owned(), Value::from(-1_i64));
    let mut turn_by_turn = Dictionary::new();
    turn_by_turn.insert("appStateID".to_owned(), Value::from(3_u64));
    turn_by_turn.insert("state".to_owned(), Value::Boolean(false));
    let mut resources = vec![resource(1_u64)];
    if audio {
        resources.push(resource(2_u64));
    }
    let mut modes = Dictionary::new();
    modes.insert("resources".to_owned(), Value::Array(resources));
    modes.insert(
        "appStates".to_owned(),
        Value::Array(vec![
            Value::Dictionary(call),
            Value::Dictionary(speech),
            Value::Dictionary(turn_by_turn),
        ]),
    );
    modes
}

fn main_display(config: &AirPlayConfig) -> Dictionary {
    let mut display = Dictionary::new();
    display.insert(
        "uuid".to_owned(),
        Value::String(MAIN_DISPLAY_UUID.to_owned()),
    );
    display.insert("type".to_owned(), Value::from(110_u64));
    display.insert(
        "maxFPS".to_owned(),
        Value::from(u64::from(config.display_max_fps)),
    );
    display.insert("widthPixels".to_owned(), Value::from(DISPLAY_WIDTH_PIXELS));
    display.insert(
        "heightPixels".to_owned(),
        Value::from(DISPLAY_HEIGHT_PIXELS),
    );
    display.insert(
        "widthPhysical".to_owned(),
        Value::from(u64::from(config.display_width_physical_mm)),
    );
    display.insert(
        "heightPhysical".to_owned(),
        Value::from(u64::from(config.display_height_physical_mm)),
    );
    display.insert("features".to_owned(), Value::from(0x0a_u64));
    display.insert("primaryInputDevice".to_owned(), Value::from(1_u64));
    let mut view_area = Dictionary::new();
    view_area.insert("widthPixels".to_owned(), Value::from(DISPLAY_WIDTH_PIXELS));
    view_area.insert(
        "heightPixels".to_owned(),
        Value::from(DISPLAY_HEIGHT_PIXELS),
    );
    view_area.insert("originXPixels".to_owned(), Value::from(0_u64));
    view_area.insert("originYPixels".to_owned(), Value::from(0_u64));
    view_area.insert(
        "viewAreaStatusBarEdge".to_owned(),
        Value::from(config.dock.status_bar_edge()),
    );
    display.insert(
        "viewAreas".to_owned(),
        Value::Array(vec![Value::Dictionary(view_area)]),
    );
    display.insert("initialViewArea".to_owned(), Value::from(0_u64));
    display
}

fn hid_device(device: prelude_control::HidDeviceDeclaration) -> Dictionary {
    let mut dictionary = Dictionary::new();
    dictionary.insert(
        "hidProductID".to_owned(),
        Value::from(u64::from(device.product_id)),
    );
    dictionary.insert(
        "hidVendorID".to_owned(),
        Value::from(u64::from(device.vendor_id)),
    );
    dictionary.insert(
        "hidCountryCode".to_owned(),
        Value::from(u64::from(device.country_code)),
    );
    dictionary.insert("uuid".to_owned(), Value::String(device.uuid));
    dictionary.insert("name".to_owned(), Value::String(device.name.to_owned()));
    dictionary.insert("displayUUID".to_owned(), Value::String(device.display_uuid));
    dictionary.insert("hidDescriptor".to_owned(), Value::Data(device.descriptor));
    dictionary
}

#[cfg(unix)]
fn read_bounded_regular_file(path: &Path, maximum: usize, secret: bool) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::MetadataExt;

    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("opening a provisioned AirPlay file failed: {error}"))?;
    if !path_metadata.file_type().is_file() {
        return Err("provisioned AirPlay input must be a regular file, not a symlink".to_owned());
    }
    let mut file = File::open(path)
        .map_err(|error| format!("opening a provisioned AirPlay file failed: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("reading provisioned AirPlay file metadata failed: {error}"))?;
    if !metadata.file_type().is_file()
        || metadata.dev() != path_metadata.dev()
        || metadata.ino() != path_metadata.ino()
    {
        return Err("provisioned AirPlay input changed while it was opened".to_owned());
    }
    if secret {
        let mode = metadata.mode() & 0o777;
        if !matches!(mode, 0o400 | 0o600) || metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(
                "AirPlay setup-code file must be owned by the daemon user with mode 0400 or 0600"
                    .to_owned(),
            );
        }
    }
    let length = usize::try_from(metadata.len())
        .map_err(|_| "provisioned AirPlay file is too large".to_owned())?;
    if length == 0 || length > maximum {
        return Err(format!(
            "provisioned AirPlay file must contain 1..={maximum} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(length);
    Read::by_ref(&mut file)
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading a provisioned AirPlay file failed: {error}"))?;
    if bytes.len() != length || bytes.len() > maximum {
        return Err("provisioned AirPlay input changed while it was read".to_owned());
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_bounded_regular_file(
    _path: &Path,
    _maximum: usize,
    _secret: bool,
) -> Result<Vec<u8>, String> {
    Err("restrictive AirPlay provisioning files require Unix metadata semantics".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io::Cursor, os::unix::fs::PermissionsExt, sync::Arc};

    use platform_superbird::{LocalInputAction, LocalPreset, MonotonicMicros};
    use prelude_control::{AirPlayEventKind, HidDeviceKind, SiriAction};
    use prelude_net::{
        ControlKeys, HeaderMap, MfiSigner, ReceiverEvent, RtspResponse, ScreenDescriptor,
        ScreenKind, SignerError,
    };
    use tempfile::TempDir;

    use crate::{CarPlayDock, DaemonConfig, RuntimeSettings};

    use super::*;

    fn detached_data_channels() -> DataChannels {
        DataChannels::new(mpsc::sync_channel(1).0, SharedIap2Tunnel::default())
    }

    #[derive(Debug)]
    struct FixedSigner;

    impl MfiSigner for FixedSigner {
        fn protocol_major(&self) -> u8 {
            3
        }

        fn certificate(&mut self) -> Result<Vec<u8>, SignerError> {
            Ok(vec![0x30; 607])
        }

        fn sign_digest(&mut self, _digest: &[u8; 32]) -> Result<Vec<u8>, SignerError> {
            Ok(vec![0x55; 64])
        }
    }

    #[derive(Debug)]
    struct ScriptedIo {
        read: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for ScriptedIo {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.read.read(bytes)
        }
    }

    impl Write for ScriptedIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ScriptedEventIo {
        reads: VecDeque<io::Result<Vec<u8>>>,
        written: Vec<u8>,
    }

    impl Read for ScriptedEventIo {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            match self.reads.pop_front() {
                Some(Ok(value)) => {
                    let length = value.len().min(bytes.len());
                    bytes[..length].copy_from_slice(&value[..length]);
                    if length < value.len() {
                        self.reads.push_front(Ok(value[length..].to_vec()));
                    }
                    Ok(length)
                }
                Some(Err(error)) => Err(error),
                None => Err(io::Error::new(io::ErrorKind::WouldBlock, "script idle")),
            }
        }
    }

    impl Write for ScriptedEventIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ScriptedInput {
        polls: VecDeque<LiveInputBatch>,
        cancel: LiveInputBatch,
        cancel_after_first_poll: Option<CancellationToken>,
    }

    impl LiveInputSource for ScriptedInput {
        fn poll(&mut self) -> Result<LiveInputBatch, String> {
            let batch = self
                .polls
                .pop_front()
                .ok_or_else(|| "scripted input was over-polled".to_owned())?;
            if let Some(token) = self.cancel_after_first_poll.take() {
                token.cancel();
            }
            Ok(batch)
        }

        fn cancel(&mut self) -> Result<LiveInputBatch, String> {
            Ok(self.cancel.clone())
        }
    }

    fn touch(timestamp: u64, phase: TouchInputPhase) -> PlatformInputEvent {
        PlatformInputEvent {
            timestamp: MonotonicMicros(timestamp),
            kind: PlatformInputEventKind::Touch {
                contact_id: 0,
                phase,
                x: 123,
                y: 234,
            },
        }
    }

    fn event_cipher_pair() -> (EventChannelKeys, EventChannelCipher) {
        (
            EventChannelKeys::new([0xaa; 32], [0xbb; 32]).expect("accessory keys"),
            EventChannelCipher::new(
                EventChannelKeys::new([0xbb; 32], [0xaa; 32]).expect("phone keys"),
            )
            .expect("phone cipher"),
        )
    }

    #[test]
    fn display_status_tracks_the_receiver_until_recording() {
        use crate::{ConnectionStep, DisplayStatus};

        let wifi = ConnectionStep::WaitingForIphoneOnWifi;
        assert_eq!(
            AirPlayStage::Listening.display_status(wifi),
            DisplayStatus::Connecting(Some(wifi))
        );
        assert_eq!(
            AirPlayStage::Connected.display_status(wifi),
            DisplayStatus::Connecting(Some(ConnectionStep::VerifyingPairing))
        );
        assert_eq!(
            AirPlayStage::StreamsConfigured.display_status(wifi),
            DisplayStatus::Connecting(Some(ConnectionStep::StartingStream))
        );
        assert_eq!(
            AirPlayStage::Recording.display_status(wifi),
            DisplayStatus::Connected
        );
        assert_eq!(
            AirPlayStage::Failed.display_status(wifi),
            DisplayStatus::Connecting(None)
        );
    }

    #[test]
    fn event_channel_preserves_keys_that_arrive_before_the_connection() {
        let mut accepted = None;
        let mut keys = Some(EventChannelKeys::new([0x11; 32], [0x22; 32]).expect("event keys"));

        assert!(take_ready_event_channel::<u8>(&mut accepted, &mut keys).is_none());
        assert!(keys.is_some(), "unmatched event keys must remain pending");

        accepted = Some(7);
        let (stream, _keys) =
            take_ready_event_channel(&mut accepted, &mut keys).expect("ready channel");
        assert_eq!(stream, 7);
        assert!(accepted.is_none());
        assert!(keys.is_none());
    }

    #[test]
    fn event_channel_preserves_connection_that_arrives_before_the_keys() {
        let mut accepted = Some(9);
        let mut keys = None;

        assert!(take_ready_event_channel(&mut accepted, &mut keys).is_none());
        assert_eq!(
            accepted,
            Some(9),
            "unmatched connection must remain pending"
        );

        keys = Some(EventChannelKeys::new([0x33; 32], [0x44; 32]).expect("event keys"));
        let (stream, _keys) =
            take_ready_event_channel(&mut accepted, &mut keys).expect("ready channel");
        assert_eq!(stream, 9);
        assert!(accepted.is_none());
        assert!(keys.is_none());
    }

    fn progress() -> SharedProgress {
        Arc::new(Mutex::new(AirPlayProgress {
            stage: AirPlayStage::Connected,
            event_count: 0,
            negotiated_streams: 0,
            event_channel_ready: false,
            input_ready: false,
            input_event_count: 0,
            local_event_count: 0,
            media_ready: false,
            audio_ready: false,
            microphone_ready: false,
            media_recording: false,
            bluetooth_handoff_requested: false,
        }))
    }

    fn request() -> RtspRequest {
        RtspRequest {
            method: "OPTIONS".to_owned(),
            target: "*".to_owned(),
            version: "RTSP/1.0".to_owned(),
            headers: HeaderMap::from([("cseq".to_owned(), "1".to_owned())]),
            body: Vec::new(),
        }
    }

    #[test]
    fn carplay_duck_commands_preserve_exact_gain_and_duration_across_siri_modes() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let info = InfoPayload::from_dictionary(Dictionary::new(), PlistLimits::default())
            .expect("empty info");
        let receiver = AirPlayReceiver::new(
            ReceiverConfig::new("2468", identity.pairing_id(), info),
            store,
            identity,
            FixedSigner,
        )
        .expect("receiver");
        let ports = SharedBoundPorts::new(
            BoundPorts::bind(&ScopedBindAddress::test_loopback()).expect("ports"),
        );
        let encode_command = |command_type: &str, params: Dictionary| {
            let mut command = Dictionary::new();
            command.insert("type".to_owned(), Value::String(command_type.to_owned()));
            command.insert("params".to_owned(), Value::Dictionary(params));
            let mut body = Vec::new();
            Value::Dictionary(command)
                .to_writer_binary(&mut body)
                .expect("command plist");
            body
        };
        let mut duck = Dictionary::new();
        duck.insert("durationMs".to_owned(), Value::from(240_u64));
        duck.insert("volume".to_owned(), Value::from(-12_i64));
        let mut speech = Dictionary::new();
        speech.insert("appStateID".to_owned(), Value::from(1_u64));
        speech.insert("speechMode".to_owned(), Value::from(1_i64));
        let mut modes = Dictionary::new();
        modes.insert(
            "appStates".to_owned(),
            Value::Array(vec![Value::Dictionary(speech)]),
        );
        let mut unduck = Dictionary::new();
        unduck.insert("durationMs".to_owned(), Value::from(600_u64));
        let events = vec![
            ReceiverEvent::CommandReceived(encode_command("duckAudio", duck)),
            ReceiverEvent::CommandReceived(encode_command("modesChanged", modes)),
            ReceiverEvent::CommandReceived(encode_command("unduckAudio", unduck)),
        ];
        let media = MediaController::responding_for_test();
        let mut decoder = ControlEventDecoder::default();
        let mut screen_stream_ready = false;
        let mut recording_requested = false;
        prepare_media_output(
            &receiver,
            &ports,
            &events,
            Some(&media),
            &mut decoder,
            &mut screen_stream_ready,
            &mut recording_requested,
            &mut detached_data_channels(),
            &progress(),
        )
        .expect("process commands");

        let expected_gain = 10_f64.powf(-12.0 / 20.0);
        let ducking = media.ducking_events();
        assert_eq!(
            ducking.len(),
            2,
            "Siri mode must not replace explicit ducking"
        );
        assert!(ducking[0].0);
        assert_eq!(ducking[0].2, 240);
        assert!((ducking[0].1.expect("duck gain") - expected_gain).abs() < f64::EPSILON);
        assert_eq!(ducking[1], (false, None, 600));
    }

    #[test]
    fn rejected_control_commands_preserve_state_and_allow_following_commands() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let info = InfoPayload::from_dictionary(Dictionary::new(), PlistLimits::default())
            .expect("empty info");
        let receiver = AirPlayReceiver::new(
            ReceiverConfig::new("2468", identity.pairing_id(), info),
            store,
            identity,
            FixedSigner,
        )
        .expect("receiver");
        let ports = SharedBoundPorts::new(
            BoundPorts::bind(&ScopedBindAddress::test_loopback()).expect("ports"),
        );
        let encode_command = |command_type: &str, params: Dictionary| {
            let mut command = Dictionary::new();
            command.insert("type".to_owned(), Value::from(command_type));
            command.insert("params".to_owned(), Value::Dictionary(params));
            let mut body = Vec::new();
            Value::Dictionary(command)
                .to_writer_binary(&mut body)
                .expect("command plist");
            body
        };
        let modes = |speech_mode: i64| {
            let mut state = Dictionary::new();
            state.insert("appStateID".to_owned(), Value::from(1_u64));
            state.insert("speechMode".to_owned(), Value::from(speech_mode));
            let mut params = Dictionary::new();
            params.insert(
                "appStates".to_owned(),
                Value::Array(vec![Value::Dictionary(state)]),
            );
            params
        };
        let mut malformed_duck = Dictionary::new();
        malformed_duck.insert("durationMs".to_owned(), Value::from(-1_i64));
        let mut oversized = Dictionary::new();
        oversized.insert("data".to_owned(), Value::Data(vec![0; 65_536]));
        let mut malformed_modes = modes(-1);
        malformed_modes
            .get_mut("appStates")
            .and_then(Value::as_array_mut)
            .expect("app states")
            .push(Value::Boolean(false));
        let rejected = [
            encode_command("duckAudio", malformed_duck),
            encode_command("newCommand", oversized),
            encode_command("modesChanged", malformed_modes),
        ];
        for body in rejected {
            let media = MediaController::responding_for_test();
            let mut decoder = ControlEventDecoder::default();
            decoder
                .decode(&encode_command("modesChanged", modes(1)))
                .expect("initial speech state");
            let mut screen_stream_ready = false;
            let mut recording_requested = false;
            prepare_media_output(
                &receiver,
                &ports,
                &[ReceiverEvent::CommandReceived(body.clone())],
                Some(&media),
                &mut decoder,
                &mut screen_stream_ready,
                &mut recording_requested,
                &mut detached_data_channels(),
                &progress(),
            )
            .expect("invalid command must not end the session");
            assert!(
                decoder.speech_active(),
                "rejection must preserve speech state"
            );
            assert!(media.ducking_events().is_empty());
            assert!(!screen_stream_ready);
            assert!(!recording_requested);

            let mut duck = Dictionary::new();
            duck.insert("volume".to_owned(), Value::from(-20_i64));
            let events = [
                ReceiverEvent::CommandReceived(encode_command("duckAudio", duck)),
                ReceiverEvent::CommandReceived(body),
                ReceiverEvent::CommandReceived(encode_command("unduckAudio", Dictionary::new())),
                ReceiverEvent::CommandReceived(encode_command("modesChanged", modes(-1))),
            ];
            prepare_media_output(
                &receiver,
                &ports,
                &events,
                Some(&media),
                &mut decoder,
                &mut screen_stream_ready,
                &mut recording_requested,
                &mut detached_data_channels(),
                &progress(),
            )
            .expect("valid commands after rejection must still execute");
            assert_eq!(
                media.ducking_events(),
                vec![(true, Some(0.1), 0), (false, None, 0)]
            );
            assert!(!decoder.speech_active());
        }
    }

    #[test]
    fn setup_code_requires_an_owner_only_regular_file_and_is_never_debugged() {
        let temporary = TempDir::new().expect("temporary");
        let path = temporary.path().join("setup-code");
        fs::write(&path, b"2468\n").expect("write code");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");
        let code = read_setup_code(&path).expect("setup code");
        assert_eq!(code, "2468");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(read_setup_code(&path).is_err());
        assert!(!format!("{:?}", AirPlayConfig::default()).contains("2468"));
    }

    #[test]
    fn setup_code_rejects_extra_lines_and_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = TempDir::new().expect("temporary");
        let path = temporary.path().join("setup-code");
        fs::write(&path, b"2468\n1357\n").expect("write code");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");
        assert!(read_setup_code(&path).is_err());
        let link = temporary.path().join("setup-link");
        symlink(&path, &link).expect("symlink");
        assert!(read_setup_code(&link).is_err());
    }

    #[test]
    fn deterministic_info_semantically_matches_external_no_audio_golden() {
        let config = AirPlayConfig::default();
        let identity = Iap2Config {
            accessory_name: Some("Prelude".to_owned()),
            model_identifier: Some("Prelude".to_owned()),
            manufacturer: Some("Vanta Labs".to_owned()),
            ..Iap2Config::default()
        };
        let payload = build_info_payload(
            &config,
            &identity,
            [0x02, 0xab, 0xcd, 0x33, 0x44, 0x55],
            true,
            ReceiverMediaClaims {
                screen: true,
                audio: false,
                microphone: false,
            },
        )
        .expect("info");
        let decoded = Value::from_reader(Cursor::new(payload.as_bytes()))
            .expect("binary plist")
            .into_dictionary()
            .expect("dictionary");
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/airplay-info-no-audio.json"))
                .expect("golden");
        let expected = golden.as_object().expect("golden object");

        for key in ["sourceVersion", "model", "manufacturer", "deviceID", "name"] {
            assert_eq!(
                decoded.get(key).and_then(Value::as_string),
                expected.get(key).and_then(serde_json::Value::as_str),
                "{key}"
            );
        }
        for key in ["features", "statusFlags"] {
            assert_eq!(
                decoded.get(key).and_then(Value::as_unsigned_integer),
                expected.get(key).and_then(serde_json::Value::as_u64),
                "{key}"
            );
        }
        for key in [
            "rightHandDrive",
            "keepAliveLowPower",
            "keepAliveSendStatsAsBody",
        ] {
            assert_eq!(
                decoded.get(key).and_then(Value::as_boolean),
                expected.get(key).and_then(serde_json::Value::as_bool),
                "{key}"
            );
        }
        assert!(!decoded.contains_key("audioFormats"));
        assert!(!decoded.contains_key("audioLatencies"));
        let string_array = |key: &str| {
            decoded
                .get(key)
                .and_then(Value::as_array)
                .expect("plist string array")
                .iter()
                .map(|value| value.as_string().expect("plist string"))
                .collect::<Vec<_>>()
        };
        let expected_extended = expected
            .get("extendedFeatures")
            .and_then(serde_json::Value::as_array)
            .expect("golden extended features")
            .iter()
            .map(|value| value.as_str().expect("golden string"))
            .collect::<Vec<_>>();
        assert_eq!(string_array("extendedFeatures"), expected_extended);
        assert_eq!(
            decoded.get("oemIconVisible").and_then(Value::as_boolean),
            Some(false)
        );

        let actual_resources = decoded
            .get("modes")
            .and_then(Value::as_dictionary)
            .and_then(|modes| modes.get("resources"))
            .and_then(Value::as_array)
            .expect("mode resources")
            .iter()
            .map(|resource| {
                resource
                    .as_dictionary()
                    .and_then(|resource| resource.get("resourceID"))
                    .and_then(Value::as_unsigned_integer)
                    .expect("resource ID")
            })
            .collect::<Vec<_>>();
        let expected_resources = expected
            .get("resourceIDs")
            .and_then(serde_json::Value::as_array)
            .expect("golden resources")
            .iter()
            .map(|value| value.as_u64().expect("golden resource ID"))
            .collect::<Vec<_>>();
        assert_eq!(actual_resources, expected_resources);

        let display = decoded
            .get("displays")
            .and_then(Value::as_array)
            .and_then(|displays| displays.first())
            .and_then(Value::as_dictionary)
            .expect("display");
        let expected_display = expected
            .get("display")
            .and_then(serde_json::Value::as_object)
            .expect("golden display");
        for key in [
            "type",
            "maxFPS",
            "widthPixels",
            "heightPixels",
            "widthPhysical",
            "heightPhysical",
            "features",
            "primaryInputDevice",
        ] {
            assert_eq!(
                display.get(key).and_then(Value::as_unsigned_integer),
                expected_display
                    .get(key)
                    .and_then(serde_json::Value::as_u64),
                "display.{key}"
            );
        }
        assert_eq!(
            display.get("uuid").and_then(Value::as_string),
            expected_display
                .get("uuid")
                .and_then(serde_json::Value::as_str)
        );
        assert_eq!(
            display
                .get("initialViewArea")
                .and_then(Value::as_unsigned_integer),
            expected_display
                .get("initialViewArea")
                .and_then(serde_json::Value::as_u64)
        );
        let view_area = display
            .get("viewAreas")
            .and_then(Value::as_array)
            .and_then(|areas| areas.first())
            .and_then(Value::as_dictionary)
            .expect("main display view area");
        let expected_view_area = expected_display
            .get("viewArea")
            .and_then(serde_json::Value::as_object)
            .expect("golden view area");
        for key in [
            "widthPixels",
            "heightPixels",
            "originXPixels",
            "originYPixels",
            "viewAreaStatusBarEdge",
        ] {
            assert_eq!(
                view_area.get(key).and_then(Value::as_unsigned_integer),
                expected_view_area
                    .get(key)
                    .and_then(serde_json::Value::as_u64),
                "display.viewArea.{key}"
            );
        }

        let actual_hid = decoded
            .get("hidDevices")
            .and_then(Value::as_array)
            .expect("HID array");
        let declarations = prelude_control::carplay_hid_devices(MAIN_DISPLAY_UUID);
        assert_eq!(actual_hid.len(), declarations.len());
        let expected_hid_uuids = expected
            .get("hidUUIDs")
            .and_then(serde_json::Value::as_array)
            .expect("golden HID UUIDs");
        for ((actual, declaration), expected_uuid) in
            actual_hid.iter().zip(declarations).zip(expected_hid_uuids)
        {
            let actual = actual.as_dictionary().expect("HID dictionary");
            assert_eq!(
                actual.get("uuid").and_then(Value::as_string),
                Some(declaration.uuid.as_str())
            );
            assert_eq!(
                actual.get("uuid").and_then(Value::as_string),
                expected_uuid.as_str()
            );
            assert_eq!(
                actual.get("hidDescriptor").and_then(Value::as_data),
                Some(declaration.descriptor.as_slice())
            );
        }
    }

    #[test]
    fn customized_dpi_is_advertised_in_carplay_info() {
        let mut config = DaemonConfig::default();
        RuntimeSettings {
            reported_dpi: 220,
            original_dpi: 170,
            dpi_customized: false,
            ..RuntimeSettings::default()
        }
        .apply_to_config(&mut config)
        .expect("apply settings");
        let identity = Iap2Config {
            accessory_name: Some("Prelude".to_owned()),
            model_identifier: Some("Prelude".to_owned()),
            manufacturer: Some("Vanta Labs".to_owned()),
            ..Iap2Config::default()
        };
        let payload = build_info_payload(
            &config.airplay,
            &identity,
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            false,
            ReceiverMediaClaims {
                screen: true,
                audio: false,
                microphone: false,
            },
        )
        .expect("info");
        let decoded = Value::from_reader(Cursor::new(payload.as_bytes()))
            .expect("binary plist")
            .into_dictionary()
            .expect("dictionary");
        let display = decoded
            .get("displays")
            .and_then(Value::as_array)
            .and_then(|displays| displays.first())
            .and_then(Value::as_dictionary)
            .expect("display");

        assert_eq!(
            display
                .get("widthPhysical")
                .and_then(Value::as_unsigned_integer),
            Some(92)
        );
        assert_eq!(
            display
                .get("heightPhysical")
                .and_then(Value::as_unsigned_integer),
            Some(55)
        );
    }

    #[test]
    fn every_dock_choice_declares_an_explicit_status_bar_edge() {
        let identity = Iap2Config {
            accessory_name: Some("Prelude".to_owned()),
            model_identifier: Some("Prelude".to_owned()),
            manufacturer: Some("Vanta Labs".to_owned()),
            ..Iap2Config::default()
        };
        for (dock, right_hand_drive, edge) in [
            (CarPlayDock::Left, false, 2),
            (CarPlayDock::Right, true, 2),
            (CarPlayDock::Bottom, false, 1),
        ] {
            let config = AirPlayConfig {
                dock,
                ..AirPlayConfig::default()
            };
            let payload = build_info_payload(
                &config,
                &identity,
                [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
                false,
                ReceiverMediaClaims {
                    screen: true,
                    audio: false,
                    microphone: false,
                },
            )
            .expect("info");
            let decoded = Value::from_reader(Cursor::new(payload.as_bytes()))
                .expect("binary plist")
                .into_dictionary()
                .expect("dictionary");
            assert_eq!(
                decoded.get("rightHandDrive").and_then(Value::as_boolean),
                Some(right_hand_drive),
                "{dock:?}"
            );
            let view_area = decoded
                .get("displays")
                .and_then(Value::as_array)
                .and_then(|displays| displays.first())
                .and_then(Value::as_dictionary)
                .and_then(|display| display.get("viewAreas"))
                .and_then(Value::as_array)
                .and_then(|areas| areas.first())
                .and_then(Value::as_dictionary)
                .expect("main display view area");
            assert_eq!(
                view_area
                    .get("viewAreaStatusBarEdge")
                    .and_then(Value::as_unsigned_integer),
                Some(edge),
                "{dock:?}"
            );
        }
    }

    #[test]
    fn info_omits_hid_declarations_until_input_worker_is_ready() {
        let identity = Iap2Config {
            accessory_name: Some("Prelude".to_owned()),
            model_identifier: Some("Prelude".to_owned()),
            manufacturer: Some("Vanta Labs".to_owned()),
            ..Iap2Config::default()
        };
        let info = build_info_payload(
            &AirPlayConfig::default(),
            &identity,
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            false,
            ReceiverMediaClaims {
                screen: true,
                audio: false,
                microphone: false,
            },
        )
        .expect("info");
        let decoded = Value::from_reader(Cursor::new(info.as_bytes()))
            .expect("plist")
            .into_dictionary()
            .expect("dictionary");
        assert!(!decoded.contains_key("hidDevices"));
    }

    #[test]
    fn info_advertises_backend_formats_and_required_neutral_latencies() {
        let identity = Iap2Config {
            accessory_name: Some("Prelude".to_owned()),
            model_identifier: Some("Prelude".to_owned()),
            manufacturer: Some("Vanta Labs".to_owned()),
            ..Iap2Config::default()
        };
        let info = build_info_payload(
            &AirPlayConfig::default(),
            &identity,
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            true,
            ReceiverMediaClaims {
                screen: true,
                audio: true,
                microphone: true,
            },
        )
        .expect("info");
        let decoded = Value::from_reader(Cursor::new(info.as_bytes()))
            .expect("plist")
            .into_dictionary()
            .expect("dictionary");
        assert_eq!(
            decoded.get("features").and_then(Value::as_unsigned_integer),
            Some(CARPLAY_FEATURES)
        );
        let latencies = decoded
            .get("audioLatencies")
            .and_then(Value::as_array)
            .expect("audio latencies");
        assert_eq!(latencies.len(), 9);
        for latency in latencies {
            let latency = latency.as_dictionary().expect("audio latency");
            assert_eq!(
                latency
                    .get("inputLatencyMicros")
                    .and_then(Value::as_unsigned_integer),
                Some(30_000)
            );
            assert_eq!(
                latency
                    .get("outputLatencyMicros")
                    .and_then(Value::as_unsigned_integer),
                Some(171_333)
            );
        }
        let formats = decoded
            .get("audioFormats")
            .and_then(Value::as_array)
            .expect("audio formats");
        assert_eq!(formats.len(), 9);
        let tuples = formats
            .iter()
            .map(|format| {
                let format = format.as_dictionary().expect("audio format");
                (
                    format.get("type").and_then(Value::as_unsigned_integer),
                    format.get("audioType").and_then(Value::as_string),
                    format
                        .get("audioOutputFormats")
                        .and_then(Value::as_unsigned_integer),
                    format
                        .get("audioInputFormats")
                        .and_then(Value::as_unsigned_integer),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tuples,
            vec![
                (Some(100), Some("compatibility"), Some(0xc3fc), Some(0x4154)),
                (Some(101), Some("compatibility"), Some(0xc3fc), None),
                (
                    Some(100),
                    Some("default"),
                    Some(0x7000_c3fc),
                    Some(0x7000_4154),
                ),
                (Some(100), Some("alert"), Some(0x7000_c3fc), None),
                (Some(100), Some("media"), Some(0xc3fc), None),
                (
                    Some(100),
                    Some("telephony"),
                    Some(0x7000_4154),
                    Some(0x7000_4154),
                ),
                (
                    Some(100),
                    Some("speechRecognition"),
                    Some(0x7000_4154),
                    Some(0x7000_4154),
                ),
                (Some(101), Some("default"), Some(0x7000_c3fc), None),
                (Some(102), Some("media"), Some(0x80_0000), None),
            ]
        );
    }

    #[test]
    fn screen_only_profile_uses_pinned_no_audio_capability_declaration() {
        let identity = Iap2Config {
            accessory_name: Some("Prelude".to_owned()),
            model_identifier: Some("Prelude".to_owned()),
            manufacturer: Some("Vanta Labs".to_owned()),
            ..Iap2Config::default()
        };
        let config = AirPlayConfig {
            disable_audio_output: true,
            ..AirPlayConfig::default()
        };
        let backend = ReceiverMediaClaims {
            screen: true,
            audio: true,
            microphone: true,
        };
        let advertised = ReceiverMediaClaims {
            screen: backend.screen,
            audio: backend.audio && !config.disable_audio_output,
            microphone: backend.microphone && !config.disable_audio_output,
        };
        let info = build_info_payload(
            &config,
            &identity,
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            true,
            advertised,
        )
        .expect("info");
        let decoded = Value::from_reader(Cursor::new(info.as_bytes()))
            .expect("plist")
            .into_dictionary()
            .expect("dictionary");
        assert_eq!(
            decoded.get("features").and_then(Value::as_unsigned_integer),
            Some(CARPLAY_FEATURES_NO_AUDIO)
        );
        assert!(!decoded.contains_key("audioFormats"));
        assert!(!decoded.contains_key("audioLatencies"));
        assert!(decoded.contains_key("displays"));
        let resources = decoded
            .get("modes")
            .and_then(Value::as_dictionary)
            .and_then(|modes| modes.get("resources"))
            .and_then(Value::as_array)
            .expect("mode resources")
            .iter()
            .map(|resource| {
                resource
                    .as_dictionary()
                    .and_then(|resource| resource.get("resourceID"))
                    .and_then(Value::as_unsigned_integer)
                    .expect("resource ID")
            })
            .collect::<Vec<_>>();
        assert_eq!(resources, [1], "main audio must stay with the phone");

        let progress = initial_airplay_progress(advertised, true);
        assert!(progress.media_ready, "screen backend remains available");
        assert!(!progress.audio_ready, "audio must not be claimed");
        assert!(
            !progress.microphone_ready,
            "microphone must not be claimed without the audio route"
        );
        assert!(progress.input_ready);
    }

    #[test]
    fn start_metadata_uses_host_mac_pairing_id_and_pinned_source_version() {
        let temporary = TempDir::new().expect("temporary");
        let store = PairingStore::open(temporary.path()).expect("store");
        let identity = store.load_or_create_identity().expect("identity");
        let metadata =
            receiver_start_metadata(7000, [0x02, 0xab, 0xcd, 0x33, 0x44, 0x55], &identity);
        assert_eq!(metadata.service_port, 7000);
        assert_eq!(metadata.device_identifier, "02:AB:CD:33:44:55");
        assert_eq!(metadata.public_key, identity.pairing_id());
        assert_ne!(metadata.public_key, hex::encode(identity.public_key()));
        assert_eq!(metadata.source_version, "320.17");
        let debug = format!("{metadata:?}");
        assert!(!debug.contains(identity.pairing_id()));
        assert!(!debug.contains("02:AB:CD:33:44:55"));
    }

    #[test]
    fn pairing_identity_is_persisted_under_state_dir_with_private_permissions() {
        let temporary = TempDir::new().expect("temporary");
        let directory = pairing_directory(temporary.path());
        let store = PairingStore::open(&directory).expect("store");
        store.load_or_create_identity().expect("identity");
        assert_eq!(store.directory(), directory);
        assert_eq!(
            fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("identity.json"))
                .expect("identity metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn port_provider_returns_only_prebound_scoped_sockets() {
        let address = ScopedBindAddress::test_loopback();
        let mut ports = BoundPorts::bind(&address).expect("bound ports");
        let session = ports
            .session_ports(&TimingDescriptor {
                peer_timing_port: None,
                low_power_keepalive: true,
                peer: prelude_net::PeerIdentity {
                    name: None,
                    device_id: None,
                    wifi_mac_address: None,
                    model: None,
                },
            })
            .expect("session ports");
        assert_ne!(session.timing_port, session.event_port);
        assert!(session.keep_alive_port.is_some());
        assert_eq!(ports.available_screen.len(), 16);
        assert_eq!(ports.available_audio.len(), 16);
    }

    #[test]
    fn negotiated_socket_seams_transfer_each_owner_exactly_once() {
        let address = ScopedBindAddress::test_loopback();
        let mut ports = SharedBoundPorts::new(BoundPorts::bind(&address).expect("bound ports"));
        let _event = ports.take_event_listener().expect("event listener");
        ports
            .session_ports(&TimingDescriptor {
                peer_timing_port: None,
                low_power_keepalive: false,
                peer: prelude_net::PeerIdentity {
                    name: None,
                    device_id: None,
                    wifi_mac_address: None,
                    model: None,
                },
            })
            .expect("session ports");
        assert!(ports.take_timing_socket().expect("timing").is_some());
        assert!(ports.take_timing_socket().expect("timing twice").is_none());

        let screen = StreamDescriptor::Screen(ScreenDescriptor {
            kind: ScreenKind::Main,
            connection_id: 42,
        });
        ports.stream_ports(&screen).expect("screen port");
        assert!(matches!(
            ports.take_media_sockets(screen.key()).expect("screen"),
            Some(OwnedMediaSockets::Screen(_))
        ));
        assert!(
            ports
                .take_media_sockets(screen.key())
                .expect("screen twice")
                .is_none()
        );
    }

    #[test]
    fn partial_teardown_replenishes_prebound_audio_pool_without_a_lifetime_cap() {
        let address = ScopedBindAddress::test_loopback();
        let mut ports = SharedBoundPorts::new(BoundPorts::bind(&address).expect("bound ports"));
        let mut advertised = BTreeSet::new();
        for connection_id in 0..u64::try_from(MAXIMUM_PREBOUND_STREAMS_PER_KIND * 4)
            .expect("bounded connection count")
        {
            let stream = StreamDescriptor::Audio(prelude_net::AudioDescriptor {
                kind: prelude_net::AudioStreamKind::Main,
                connection_id,
                audio_type: "default".to_owned(),
                audio_format: 0x8000,
                audio_latency_ms: None,
                frames_per_packet: None,
                microphone: None,
            });
            let StreamPorts::Audio {
                data_port,
                control_port,
            } = ports.stream_ports(&stream).expect("audio ports")
            else {
                panic!("audio stream received non-audio ports")
            };
            assert!(advertised.insert(data_port));
            assert!(advertised.insert(control_port));
            let worker = ports
                .take_media_sockets(stream.key())
                .expect("claim audio sockets")
                .expect("audio sockets");
            assert!(
                ports
                    .take_media_sockets(stream.key())
                    .expect("duplicate claim")
                    .is_none()
            );
            drop(worker);
            ports
                .release_media_sockets(&[stream.key()])
                .expect("replenish audio sockets");
        }
        let owner = ports.0.lock().expect("socket owner");
        assert_eq!(
            owner.available_audio.len(),
            MAXIMUM_PREBOUND_STREAMS_PER_KIND
        );
        assert!(owner.streams.is_empty());
        assert!(owner.claimed_streams.is_empty());
    }

    #[test]
    fn peer_policy_rejects_unscoped_and_non_link_local_addresses() {
        let endpoint = ScopedBindAddress {
            address: "fe80::1".parse().expect("address"),
            scope_id: 7,
        };
        assert!(endpoint.accepts_peer(SocketAddr::V6(SocketAddrV6::new(
            "fe80::2".parse().expect("peer"),
            7000,
            0,
            7,
        ))));
        assert!(!endpoint.accepts_peer(SocketAddr::V6(SocketAddrV6::new(
            "fe80::2".parse().expect("peer"),
            7000,
            0,
            0,
        ))));
        assert!(!endpoint.accepts_peer(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            7000,
            0,
            7,
        ))));
    }

    #[test]
    fn scripted_cleartext_receiver_drives_real_rtsp_codec_and_response() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let info = InfoPayload::from_dictionary(
            plist::Dictionary::from_iter([(
                "features".to_owned(),
                plist::Value::Integer(7_u64.into()),
            )]),
            PlistLimits::default(),
        )
        .expect("info");
        let mut receiver = AirPlayReceiver::new(
            ReceiverConfig::new("2468", identity.pairing_id(), info),
            store,
            identity,
            FixedSigner,
        )
        .expect("receiver");
        let mut ports = SharedBoundPorts::new(
            BoundPorts::bind(&ScopedBindAddress::test_loopback()).expect("ports"),
        );
        let (key_sender, _key_receiver) = mpsc::sync_channel(1);
        let (command_sender, _command_receiver) = mpsc::sync_channel(1);
        let mut io = ScriptedIo {
            read: Cursor::new(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec()),
            written: Vec::new(),
        };
        let cancellation = CancellationToken::default();
        let result = run_control_connection(
            &mut io,
            &mut receiver,
            &mut ports,
            &key_sender,
            &command_sender,
            &SharedIap2Tunnel::default(),
            &progress(),
            &cancellation,
            None,
        );
        assert!(result.is_err(), "script ends by closing after its response");
        let response = String::from_utf8(io.written).expect("ASCII response");
        assert!(response.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(response.contains("cseq: 1\r\n"));
    }

    #[test]
    fn missing_first_screen_frame_requests_a_keyframe_then_fails_the_session() {
        let media = MediaController::responding_for_test();
        let (sender, receiver) = mpsc::sync_channel(1);
        let started = |ago: Duration| {
            Instant::now()
                .checked_sub(ago)
                .expect("monotonic clock has enough history")
        };

        let mut first_video = Some((
            FirstVideoWatchdog::new(started(Duration::from_secs(1))),
            media.screen_frames_received(),
        ));
        check_first_screen_frame(&mut first_video, &media, &sender).expect("still waiting");
        assert!(receiver.try_recv().is_err());

        let mut first_video = Some((
            FirstVideoWatchdog::new(started(FirstVideoWatchdog::KEYFRAME_AFTER)),
            media.screen_frames_received(),
        ));
        check_first_screen_frame(&mut first_video, &media, &sender).expect("keyframe request");
        assert!(matches!(
            receiver.try_recv(),
            Ok(EventWorkerCommand::ForceKeyFrame)
        ));

        let mut first_video = Some((
            FirstVideoWatchdog::new(started(FirstVideoWatchdog::TIMEOUT)),
            media.screen_frames_received(),
        ));
        let error = check_first_screen_frame(&mut first_video, &media, &sender)
            .expect_err("the stalled session fails");
        assert!(error.contains("no screen video"));
    }

    #[test]
    fn pair_verify_response_is_clear_before_control_cipher_is_installed() {
        let mut output = ReceiverOutput {
            response: RtspResponse::new(200, "OK"),
            events: vec![ReceiverEvent::ControlEncryptionReady {
                controller_id: "must-not-appear-in-progress".to_owned(),
            }],
            control_keys: Some(ReceiverEncryptionKeys {
                control: ControlKeys::new([0x11; 32], [0x22; 32]).expect("keys"),
                event: EventChannelKeys::new([0x33; 32], [0x44; 32]).expect("event keys"),
            }),
        };
        output.response.body = b"pair-verify-m4".to_vec();
        let mut written = Vec::new();
        let shared = progress();
        let mut cipher = None;
        let (key_sender, key_receiver) = mpsc::sync_channel(1);
        send_output(
            &mut written,
            &request(),
            output,
            &mut cipher,
            &key_sender,
            &shared,
        )
        .expect("send M4");
        assert!(key_receiver.try_recv().is_ok());
        assert!(written.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert!(written.ends_with(b"pair-verify-m4"));
        assert!(cipher.is_some());
        assert_eq!(
            shared.lock().expect("progress").stage,
            AirPlayStage::EncryptedControl
        );
        assert!(
            !format!("{:?}", shared.lock().expect("progress"))
                .contains("must-not-appear-in-progress")
        );

        let clear_length = written.len();
        let encrypted_output = ReceiverOutput {
            response: RtspResponse::new(200, "OK"),
            events: vec![ReceiverEvent::MfiAuthenticated],
            control_keys: None,
        };
        send_output(
            &mut written,
            &request(),
            encrypted_output,
            &mut cipher,
            &key_sender,
            &shared,
        )
        .expect("send encrypted response");
        assert!(!written[clear_length..].starts_with(b"RTSP/1.0"));
        let mut controller_cipher =
            ControlCipher::new(ControlKeys::new([0x22; 32], [0x11; 32]).expect("controller keys"))
                .expect("controller cipher");
        let decrypted = controller_cipher
            .decrypt(&written[clear_length..])
            .expect("decrypt response");
        assert!(decrypted.starts_with(b"RTSP/1.0 200 OK\r\n"));
        assert_eq!(
            shared.lock().expect("progress").stage,
            AirPlayStage::MfiAuthenticated
        );
    }

    #[test]
    fn malformed_rtsp_fails_closed_without_a_response() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let info = InfoPayload::from_dictionary(plist::Dictionary::new(), PlistLimits::default())
            .expect("info");
        let mut receiver = AirPlayReceiver::new(
            ReceiverConfig::new("2468", identity.pairing_id(), info),
            store,
            identity,
            FixedSigner,
        )
        .expect("receiver");
        let mut ports = SharedBoundPorts::new(
            BoundPorts::bind(&ScopedBindAddress::test_loopback()).expect("ports"),
        );
        let (key_sender, _key_receiver) = mpsc::sync_channel(1);
        let (command_sender, _command_receiver) = mpsc::sync_channel(1);
        let mut io = ScriptedIo {
            read: Cursor::new(b"options * RTSP/1.0\r\n\r\n".to_vec()),
            written: Vec::new(),
        };
        assert!(
            run_control_connection(
                &mut io,
                &mut receiver,
                &mut ports,
                &key_sender,
                &command_sender,
                &SharedIap2Tunnel::default(),
                &progress(),
                &CancellationToken::default(),
                None,
            )
            .is_err()
        );
        assert!(io.written.is_empty());
    }

    #[test]
    fn encrypted_event_worker_acks_phone_and_delivers_full_terminal_batch_in_order() {
        let (keys, mut phone_cipher) = event_cipher_pair();
        let phone_request = phone_cipher
            .encrypt(b"POST /command RTSP/1.0\r\nCSeq: 77\r\nContent-Length: 0\r\n\r\n")
            .expect("phone request");
        let mut io = ScriptedEventIo {
            reads: VecDeque::from([Ok(phone_request)]),
            written: Vec::new(),
        };
        let input = ScriptedInput {
            polls: VecDeque::from([LiveInputBatch {
                events: vec![
                    touch(1, TouchInputPhase::Down),
                    PlatformInputEvent {
                        timestamp: MonotonicMicros(2),
                        kind: PlatformInputEventKind::Local(LocalInputAction::Mute),
                    },
                    touch(3, TouchInputPhase::Up),
                ],
                terminal: Some(InputTerminal::DeviceUnavailable),
            }]),
            cancel: LiveInputBatch {
                events: Vec::new(),
                terminal: Some(InputTerminal::Cancelled),
            },
            cancel_after_first_poll: None,
        };
        let shared = progress();
        let (_command_sender, command_receiver) = mpsc::sync_channel(1);
        let media = MediaController::responding_for_test();
        let result = run_event_connection(
            &mut io,
            keys,
            &command_receiver,
            Some(Box::new(input)),
            &shared,
            &CancellationToken::default(),
            Some(&media),
        );
        assert!(result.is_err(), "device loss is terminal");
        let plaintext = phone_cipher.decrypt(&io.written).expect("phone decrypt");
        let messages = EventChannelCodec::new(RtspLimits::default())
            .expect("codec")
            .push(&plaintext)
            .expect("messages");
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[0],
            EventChannelMessage::Response(response)
                if response.status == 200 && response.headers.get("cseq") == Some(&"77".to_owned())
        ));
        assert!(
            matches!(&messages[1], EventChannelMessage::Request(request) if request.target == "/command")
        );
        assert!(
            matches!(&messages[2], EventChannelMessage::Request(request) if request.target == "/command")
        );
        let progress = shared.lock().expect("progress");
        assert_eq!(progress.input_event_count, 2);
        assert_eq!(progress.local_event_count, 1);
        assert!(!progress.input_ready);
    }

    #[test]
    fn event_response_without_cseq_correlates_to_oldest_pending_command() {
        let (keys, _) = event_cipher_pair();
        let mut cipher = EventChannelCipher::new(keys).expect("cipher");
        let response = EventChannelMessage::Response(RtspResponse::new(200, "OK"));
        let mut pending = BTreeSet::from([7]);
        handle_event_message(&mut Vec::new(), &mut cipher, response.clone(), &mut pending)
            .expect("single pending command");
        assert!(pending.is_empty());

        let mut ordered = BTreeSet::from([7, 8]);
        handle_event_message(&mut Vec::new(), &mut cipher, response, &mut ordered)
            .expect("oldest pending command");
        assert_eq!(ordered, BTreeSet::from([8]));
    }

    #[test]
    fn recording_sends_pinned_initial_vehicle_state_before_waiting_for_input() {
        let (keys, mut phone_cipher) = event_cipher_pair();
        let mut io = ScriptedEventIo {
            reads: VecDeque::new(),
            written: Vec::new(),
        };
        let input = ScriptedInput {
            polls: VecDeque::from([LiveInputBatch {
                events: Vec::new(),
                terminal: Some(InputTerminal::DeviceUnavailable),
            }]),
            cancel: LiveInputBatch {
                events: Vec::new(),
                terminal: Some(InputTerminal::Cancelled),
            },
            cancel_after_first_poll: None,
        };
        let shared = progress();
        let (command_sender, command_receiver) = mpsc::sync_channel(1);
        let (completed, completion) = mpsc::sync_channel(1);
        command_sender
            .send(EventWorkerCommand::SetNightMode {
                enabled: None,
                completed,
            })
            .expect("record command");

        assert!(
            run_event_connection(
                &mut io,
                keys,
                &command_receiver,
                Some(Box::new(input)),
                &shared,
                &CancellationToken::default(),
                None,
            )
            .is_err()
        );
        completion
            .recv_timeout(Duration::from_millis(10))
            .expect("command completion")
            .expect("command success");

        let plaintext = phone_cipher.decrypt(&io.written).expect("phone decrypt");
        let messages = EventChannelCodec::new(RtspLimits::default())
            .expect("codec")
            .push(&plaintext)
            .expect("messages");
        let [EventChannelMessage::Request(request)] = messages.as_slice() else {
            panic!("expected one initial vehicle-state command");
        };
        let body = Value::from_reader(Cursor::new(&request.body))
            .expect("plist")
            .into_dictionary()
            .expect("dictionary");
        assert_eq!(
            body.get("type").and_then(Value::as_string),
            Some("setNightMode")
        );
        assert_eq!(
            body.get("params")
                .and_then(Value::as_dictionary)
                .and_then(|params| params.get("nightMode"))
                .and_then(Value::as_boolean),
            Some(false)
        );
    }

    #[test]
    fn platform_translation_covers_knob_rotary_and_siri() {
        let mut wire = InputWireSession::new();
        let knob = encode_platform_input(
            &mut wire,
            PlatformInputEvent {
                timestamp: MonotonicMicros(1),
                kind: PlatformInputEventKind::KnobButton {
                    button: KnobInputButton::Select,
                    pressed: true,
                },
            },
            &PresetBindings::default(),
        )
        .expect("knob");
        assert!(matches!(
            knob,
            InputDisposition::CarPlay {
                first: AirPlayEvent {
                    kind: AirPlayEventKind::Hid {
                        device: HidDeviceKind::Knob,
                        ..
                    },
                    ..
                },
                second: None,
            }
        ));
        let rotary = encode_platform_input(
            &mut wire,
            PlatformInputEvent {
                timestamp: MonotonicMicros(2),
                kind: PlatformInputEventKind::Rotary {
                    direction: RotaryDirection::CounterClockwise,
                    steps: 3,
                },
            },
            &PresetBindings::default(),
        )
        .expect("rotary");
        assert!(matches!(
            rotary,
            InputDisposition::CarPlay { second: None, .. }
        ));
        let siri = encode_platform_input(
            &mut wire,
            PlatformInputEvent {
                timestamp: MonotonicMicros(3),
                kind: PlatformInputEventKind::SiriButton { pressed: true },
            },
            &PresetBindings::default(),
        )
        .expect("Siri");
        assert!(matches!(
            siri,
            InputDisposition::CarPlay {
                first: AirPlayEvent {
                    kind: AirPlayEventKind::Siri(SiriAction::ButtonDown),
                    ..
                },
                second: None,
            }
        ));
        let cancel = encode_platform_input(
            &mut wire,
            PlatformInputEvent {
                timestamp: MonotonicMicros(4),
                kind: PlatformInputEventKind::VoiceAssistantCancel,
            },
            &PresetBindings::default(),
        )
        .expect("voice cancellation");
        assert!(matches!(
            cancel,
            InputDisposition::CarPlay {
                first: AirPlayEvent {
                    kind: AirPlayEventKind::Siri(SiriAction::ButtonUp),
                    ..
                },
                second: None,
            }
        ));
    }

    #[test]
    fn presets_map_to_exact_playback_controls_and_local_mute() {
        let mut wire = InputWireSession::new();
        for (timestamp, preset, expected) in [
            (1, LocalPreset::ONE, MediaButton::Previous),
            (2, LocalPreset::TWO, MediaButton::PlayPause),
            (3, LocalPreset::THREE, MediaButton::Next),
        ] {
            let disposition = encode_platform_input(
                &mut wire,
                PlatformInputEvent {
                    timestamp: MonotonicMicros(timestamp),
                    kind: PlatformInputEventKind::Local(LocalInputAction::Preset(preset)),
                },
                &PresetBindings::default(),
            )
            .expect("playback preset");
            let InputDisposition::CarPlay {
                first:
                    AirPlayEvent {
                        kind:
                            AirPlayEventKind::Hid {
                                device: pressed_device,
                                report: pressed_report,
                            },
                        ..
                    },
                second:
                    Some(AirPlayEvent {
                        kind:
                            AirPlayEventKind::Hid {
                                device: released_device,
                                report: released_report,
                            },
                        ..
                    }),
            } = disposition
            else {
                panic!("playback preset must produce a complete CarPlay media click");
            };
            assert_eq!(pressed_device, HidDeviceKind::Media);
            assert_eq!(pressed_report, vec![expected as u8]);
            assert_eq!(released_device, HidDeviceKind::Media);
            assert_eq!(released_report, vec![0]);
        }

        let mute = encode_platform_input(
            &mut wire,
            PlatformInputEvent {
                timestamp: MonotonicMicros(4),
                kind: PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::FOUR)),
            },
            &PresetBindings::default(),
        )
        .expect("mute preset");
        assert!(matches!(mute, InputDisposition::LocalMute));
    }

    #[test]
    fn remapped_presets_click_home_siri_and_telephony_controls() {
        let mut wire = InputWireSession::new();
        let presets = PresetBindings::default();
        presets.set([
            PresetAction::Home,
            PresetAction::VoiceAssistant,
            PresetAction::AnswerCall,
            PresetAction::EndCall,
        ]);
        let mut clicks = Vec::new();
        for (timestamp, preset) in [
            LocalPreset::ONE,
            LocalPreset::TWO,
            LocalPreset::THREE,
            LocalPreset::FOUR,
        ]
        .into_iter()
        .enumerate()
        {
            let InputDisposition::CarPlay {
                first,
                second: Some(second),
            } = encode_platform_input(
                &mut wire,
                PlatformInputEvent {
                    timestamp: MonotonicMicros(timestamp as u64 + 1),
                    kind: PlatformInputEventKind::Local(LocalInputAction::Preset(preset)),
                },
                &presets,
            )
            .expect("remapped preset")
            else {
                panic!("remapped preset must produce a complete CarPlay click");
            };
            clicks.push((first.kind, second.kind));
        }
        assert_eq!(
            clicks,
            vec![
                (
                    AirPlayEventKind::Hid {
                        device: HidDeviceKind::Knob,
                        report: vec![2, 0, 0, 0],
                    },
                    AirPlayEventKind::Hid {
                        device: HidDeviceKind::Knob,
                        report: vec![0, 0, 0, 0],
                    },
                ),
                (
                    AirPlayEventKind::Siri(SiriAction::ButtonDown),
                    AirPlayEventKind::Siri(SiriAction::ButtonUp),
                ),
                (
                    AirPlayEventKind::Hid {
                        device: HidDeviceKind::Telephony,
                        report: vec![TelephonyButton::HookSwitch as u8],
                    },
                    AirPlayEventKind::Hid {
                        device: HidDeviceKind::Telephony,
                        report: vec![0],
                    },
                ),
                (
                    AirPlayEventKind::Hid {
                        device: HidDeviceKind::Telephony,
                        report: vec![TelephonyButton::Drop as u8],
                    },
                    AirPlayEventKind::Hid {
                        device: HidDeviceKind::Telephony,
                        report: vec![0],
                    },
                ),
            ]
        );
        presets.set([PresetAction::None; 4]);
        assert!(matches!(
            encode_platform_input(
                &mut wire,
                PlatformInputEvent {
                    timestamp: MonotonicMicros(5),
                    kind: PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::ONE)),
                },
                &presets,
            ),
            Ok(InputDisposition::Ignore)
        ));
    }

    #[test]
    fn preset_batch_sends_six_media_reports_and_toggles_mute_once() {
        let (keys, mut phone_cipher) = event_cipher_pair();
        let mut accessory_cipher = EventChannelCipher::new(keys).expect("accessory cipher");
        let mut output = Vec::new();
        let mut wire = InputWireSession::new();
        let mut pending = BTreeSet::new();
        let mut last_timestamp = MonotonicTimestamp(0);
        let shared = progress();
        let media = MediaController::responding_for_test();
        let events = [
            LocalPreset::ONE,
            LocalPreset::TWO,
            LocalPreset::THREE,
            LocalPreset::FOUR,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, preset)| PlatformInputEvent {
            timestamp: MonotonicMicros(index as u64 + 1),
            kind: PlatformInputEventKind::Local(LocalInputAction::Preset(preset)),
        })
        .collect();

        process_input_batch(
            &mut output,
            &mut accessory_cipher,
            &mut wire,
            LiveInputBatch {
                events,
                terminal: None,
            },
            &mut pending,
            &mut last_timestamp,
            &shared,
            Some(&media),
            &PresetBindings::default(),
            false,
        )
        .expect("preset batch");

        let plaintext = phone_cipher.decrypt(&output).expect("phone decrypt");
        let messages = EventChannelCodec::new(RtspLimits::default())
            .expect("codec")
            .push(&plaintext)
            .expect("messages");
        let reports: Vec<Vec<u8>> = messages
            .into_iter()
            .map(|message| {
                let EventChannelMessage::Request(request) = message else {
                    panic!("preset playback must send an event-channel request");
                };
                let body = Value::from_reader(Cursor::new(request.body))
                    .expect("preset plist")
                    .into_dictionary()
                    .expect("preset dictionary");
                let Some(Value::Data(report)) = body.get("hidReport") else {
                    panic!("preset playback request must contain a HID report");
                };
                report.clone()
            })
            .collect();
        assert_eq!(
            reports,
            vec![
                vec![MediaButton::Previous as u8],
                vec![0],
                vec![MediaButton::PlayPause as u8],
                vec![0],
                vec![MediaButton::Next as u8],
                vec![0],
            ]
        );
        assert_eq!(pending.len(), 6);
        assert_eq!(media.mute_toggle_attempt_count(), 1);
        let progress = shared.lock().expect("progress");
        assert_eq!(progress.input_event_count, 6);
        assert_eq!(progress.local_event_count, 1);
    }

    #[test]
    fn host_settings_capture_suppresses_carplay_input() {
        let (keys, _) = event_cipher_pair();
        let mut accessory_cipher = EventChannelCipher::new(keys).expect("accessory cipher");
        let mut output = Vec::new();
        let mut wire = InputWireSession::new();
        let mut pending = BTreeSet::new();
        let mut last_timestamp = MonotonicTimestamp(0);
        let shared = progress();
        let media = MediaController::responding_for_test();
        process_input_batch(
            &mut output,
            &mut accessory_cipher,
            &mut wire,
            LiveInputBatch {
                events: vec![
                    touch(1, TouchInputPhase::Down),
                    PlatformInputEvent {
                        timestamp: MonotonicMicros(2),
                        kind: PlatformInputEventKind::Local(LocalInputAction::Mute),
                    },
                ],
                terminal: None,
            },
            &mut pending,
            &mut last_timestamp,
            &shared,
            Some(&media),
            &PresetBindings::default(),
            true,
        )
        .expect("suppressed input batch");

        assert!(output.is_empty());
        let progress = shared.lock().expect("progress");
        assert_eq!(progress.input_event_count, 0);
        assert_eq!(progress.local_event_count, 0);
    }

    #[test]
    fn response_window_reserves_capacity_for_a_complete_media_click() {
        let pending: BTreeSet<u32> = (0..MAXIMUM_PENDING_EVENT_RESPONSES as u32 - 1).collect();
        assert!(ensure_event_response_capacity(&pending, 1).is_ok());
        assert!(ensure_event_response_capacity(&pending, 2).is_err());
    }

    #[test]
    fn cancellation_delivers_backend_and_wire_releases_before_shutdown() {
        let (keys, mut phone_cipher) = event_cipher_pair();
        let cancellation = CancellationToken::default();
        let input = ScriptedInput {
            polls: VecDeque::from([LiveInputBatch {
                events: vec![touch(1, TouchInputPhase::Down)],
                terminal: None,
            }]),
            cancel: LiveInputBatch {
                events: vec![touch(2, TouchInputPhase::Up)],
                terminal: Some(InputTerminal::Cancelled),
            },
            cancel_after_first_poll: Some(cancellation.clone()),
        };
        let mut io = ScriptedEventIo {
            reads: VecDeque::new(),
            written: Vec::new(),
        };
        let (_command_sender, command_receiver) = mpsc::sync_channel(1);
        run_event_connection(
            &mut io,
            keys,
            &command_receiver,
            Some(Box::new(input)),
            &progress(),
            &cancellation,
            None,
        )
        .expect("cancelled worker");
        let plaintext = phone_cipher.decrypt(&io.written).expect("phone decrypt");
        let messages = EventChannelCodec::new(RtspLimits::default())
            .expect("codec")
            .push(&plaintext)
            .expect("messages");
        assert_eq!(messages.len(), 2, "touch down and forced touch up");
        assert!(messages.iter().all(|message| matches!(
            message,
            EventChannelMessage::Request(request) if request.target == "/command"
        )));
    }

    #[test]
    fn quiet_phone_cannot_delay_input_beyond_the_configured_poll_bound() {
        use std::net::TcpStream;
        use std::time::Instant;

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
        let mut phone =
            TcpStream::connect(listener.local_addr().expect("address")).expect("phone connection");
        let (mut accessory, _) = listener.accept().expect("accessory connection");
        let input_poll_bound = Duration::from_millis(100);
        let read_timeout =
            bounded_event_read_timeout(Duration::from_millis(250), Some(input_poll_bound));
        assert_eq!(read_timeout, input_poll_bound);
        accessory
            .set_read_timeout(Some(read_timeout))
            .expect("read timeout");
        accessory
            .set_write_timeout(Some(Duration::from_millis(250)))
            .expect("write timeout");

        let cancellation = CancellationToken::default();
        let input = ScriptedInput {
            polls: VecDeque::from([LiveInputBatch {
                events: vec![touch(1, TouchInputPhase::Down)],
                terminal: None,
            }]),
            cancel: LiveInputBatch {
                events: vec![touch(2, TouchInputPhase::Up)],
                terminal: Some(InputTerminal::Cancelled),
            },
            cancel_after_first_poll: Some(cancellation.clone()),
        };
        let (keys, mut phone_cipher) = event_cipher_pair();
        let (_command_sender, command_receiver) = mpsc::sync_channel(1);
        let started = Instant::now();
        run_event_connection(
            &mut accessory,
            keys,
            &command_receiver,
            Some(Box::new(input)),
            &progress(),
            &cancellation,
            None,
        )
        .expect("quiet event worker");
        assert!(
            started.elapsed() <= input_poll_bound + Duration::from_millis(50),
            "quiet event read exceeded the input poll deadline"
        );
        drop(accessory);
        phone
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("phone timeout");
        let mut encrypted = Vec::new();
        phone.read_to_end(&mut encrypted).expect("phone read");
        let plaintext = phone_cipher.decrypt(&encrypted).expect("phone decrypt");
        let messages = EventChannelCodec::new(RtspLimits::default())
            .expect("codec")
            .push(&plaintext)
            .expect("messages");
        assert_eq!(messages.len(), 2, "touch down and forced touch up");
    }

    #[test]
    fn stop_cancels_and_joins_listener_before_releasing_sockets() {
        let listener =
            TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)).expect("listener");
        let address = listener.local_addr().expect("address");
        let cancellation = CancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_dropped = Arc::clone(&dropped);
        let handle = thread::spawn(move || {
            let _owned_listener = listener;
            while !thread_cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            thread_dropped.store(true, std::sync::atomic::Ordering::Release);
            Ok::<(), String>(())
        });
        let shared = progress();
        let ports = SharedBoundPorts::new(
            BoundPorts::bind(&ScopedBindAddress::test_loopback()).expect("ports"),
        );
        let runtime = AirPlayRuntime {
            metadata: ReceiverStartMetadata {
                service_port: address.port(),
                device_identifier: "redacted-test-device".to_owned(),
                public_key: "redacted-test-pairing-id".to_owned(),
                source_version: CARPLAY_SOURCE_VERSION.to_owned(),
            },
            progress: Arc::clone(&shared),
            failure: Arc::new(Mutex::new(None)),
            cancellation,
            workers: vec![handle],
            ports,
            iap2_tunnel: SharedIap2Tunnel::default(),
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            media: None,
        };
        runtime.stop().expect("runtime stop");
        assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            shared.lock().expect("progress").stage,
            AirPlayStage::Stopped
        );
        TcpListener::bind(address).expect("listener released after joined worker");
    }
}
