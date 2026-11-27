use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroU16,
    sync::Arc,
    time::Duration,
};

use plist::{Dictionary, Value};
use zeroize::Zeroizing;

use crate::{
    AccessoryIdentity, AuthSetupHandler, ControlKeys, DataStreamDirection, Error, EventChannelKeys,
    MfiSigner, PairSetup, PairSetupState, PairVerification, PairVerify, PairVerifyState,
    PairingStore, PlistLimits, Result, RtspRequest, RtspResponse,
    negotiation::{
        ALTERNATE_AUDIO_STREAM_TYPE, ALTERNATE_SCREEN_STREAM_TYPE, AudioDescriptor,
        AudioStreamKind, DATA_CHANNEL_STREAM_TYPE, DataChannelDescriptor, HIGH_AUDIO_STREAM_TYPE,
        MAIN_AUDIO_STREAM_TYPE, MAIN_SCREEN_STREAM_TYPE, MessageChannelDescriptor,
        MicrophoneDescriptor, PeerIdentity, PortProvider, ScreenDescriptor, ScreenKind,
        SessionPorts, StreamDescriptor, StreamKey, StreamPorts, TimingDescriptor,
        VIDEO_PLAYBACK_CLIENT_TYPE,
    },
    plist_codec,
};

const RECENT_CSEQ_WINDOW: usize = 64;
const PAIRING_CONTENT_TYPE: &str = "application/pairing+tlv8";
const CARPLAY_PAIRING_REQUEST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
const CARPLAY_VERIFY_REQUEST_CONTENT_TYPE: &str = "application/octet-stream";
const PLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
const TEXT_PARAMETERS_CONTENT_TYPE: &str = "text/parameters";
const PUBLIC_METHODS: &str =
    "OPTIONS, GET, POST, SETUP, RECORD, SET_PARAMETER, GET_PARAMETER, FLUSH, TEARDOWN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTransport {
    Cleartext,
    Encrypted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverState {
    Cleartext,
    AwaitingEncryptedControl,
    Encrypted,
    Authenticated,
    SessionConfigured,
    StreamsConfigured,
    Recording,
    Teardown,
    Failed,
}

impl ReceiverState {
    const fn encrypted(self) -> bool {
        matches!(
            self,
            Self::Encrypted
                | Self::Authenticated
                | Self::SessionConfigured
                | Self::StreamsConfigured
                | Self::Recording
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverLimits {
    pub plist: PlistLimits,
    pub maximum_headers: usize,
    pub maximum_header_bytes: usize,
    pub maximum_body_bytes: usize,
    pub maximum_streams: usize,
    pub maximum_parameter_fields: usize,
    pub maximum_parameter_name_bytes: usize,
    pub maximum_parameter_value_bytes: usize,
}

impl Default for ReceiverLimits {
    fn default() -> Self {
        Self {
            plist: PlistLimits::default(),
            maximum_headers: 64,
            maximum_header_bytes: 32 * 1024,
            maximum_body_bytes: 4 * 1024 * 1024,
            maximum_streams: 16,
            maximum_parameter_fields: 64,
            maximum_parameter_name_bytes: 128,
            maximum_parameter_value_bytes: 4096,
        }
    }
}

impl ReceiverLimits {
    fn validate(self) -> Result<Self> {
        self.plist.validate()?;
        if self.maximum_headers == 0
            || self.maximum_header_bytes < 16
            || self.maximum_body_bytes == 0
            || self.maximum_streams == 0
            || self.maximum_parameter_fields == 0
            || self.maximum_parameter_name_bytes == 0
            || self.maximum_parameter_value_bytes == 0
        {
            return Err(Error::InvalidRtsp("invalid receiver limits".to_owned()));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoPayload {
    encoded: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFeedbackAnchor {
    pub connection_id: u64,
    pub synchronized_ntp: u64,
    pub monotonic_nanoseconds: u64,
    pub sample_time: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFeedbackStream {
    pub stream_type: u16,
    pub sample_rate_hz: u32,
    pub anchor: Option<AudioFeedbackAnchor>,
}

impl AudioFeedbackStream {
    fn validate(self) -> Result<Self> {
        if !matches!(self.stream_type, 100..=102) {
            return Err(Error::InvalidRtsp(format!(
                "audio feedback has unsupported stream type {}",
                self.stream_type
            )));
        }
        if !(8_000..=192_000).contains(&self.sample_rate_hz) {
            return Err(Error::InvalidRtsp(format!(
                "audio feedback has invalid sample rate {}",
                self.sample_rate_hz
            )));
        }
        Ok(self)
    }
}

impl InfoPayload {
    pub fn from_binary_plist(encoded: Vec<u8>, limits: PlistLimits) -> Result<Self> {
        plist_codec::decode_dictionary(&encoded, limits)?;
        Ok(Self { encoded })
    }

    pub fn from_dictionary(dictionary: Dictionary, limits: PlistLimits) -> Result<Self> {
        let encoded = plist_codec::encode(&Value::Dictionary(dictionary), limits)?;
        Ok(Self { encoded })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.encoded
    }
}

pub struct ReceiverConfig {
    setup_code: String,
    session_id: String,
    info: InfoPayload,
    pairing_timeout: Duration,
    enabled_features: Vec<String>,
    conditional_features: Vec<(String, Dictionary)>,
    limits: ReceiverLimits,
}

impl std::fmt::Debug for ReceiverConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReceiverConfig")
            .field("setup_code", &"[redacted]")
            .field("session_id", &self.session_id)
            .field("info_bytes", &self.info.encoded.len())
            .field("pairing_timeout", &self.pairing_timeout)
            .field("enabled_features", &self.enabled_features)
            .field(
                "conditional_features",
                &self
                    .conditional_features
                    .iter()
                    .map(|(feature, _)| feature)
                    .collect::<Vec<_>>(),
            )
            .field("limits", &self.limits)
            .finish()
    }
}

impl ReceiverConfig {
    pub fn new(
        setup_code: impl Into<String>,
        session_id: impl Into<String>,
        info: InfoPayload,
    ) -> Self {
        Self {
            setup_code: setup_code.into(),
            session_id: session_id.into(),
            info,
            pairing_timeout: Duration::from_secs(15),
            enabled_features: Vec::new(),
            conditional_features: Vec::new(),
            limits: ReceiverLimits::default(),
        }
    }

    pub fn with_pairing_timeout(mut self, timeout: Duration) -> Self {
        self.pairing_timeout = timeout;
        self
    }

    pub fn with_enabled_features(mut self, features: Vec<String>) -> Self {
        self.enabled_features = features;
        self
    }

    pub fn with_conditional_feature(
        mut self,
        feature: impl Into<String>,
        info_entries: Dictionary,
    ) -> Self {
        self.conditional_features
            .push((feature.into(), info_entries));
        self
    }

    pub fn with_limits(mut self, limits: ReceiverLimits) -> Self {
        self.limits = limits;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterPacket {
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub text_values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushPoint {
    pub sequence: Option<u16>,
    pub rtp_time: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverEvent {
    ControlEncryptionReady {
        controller_id: String,
    },
    MfiAuthenticated,
    InfoRequested,
    SessionNegotiated {
        timing: TimingDescriptor,
        ports: SessionPorts,
    },
    StreamNegotiated {
        stream: StreamDescriptor,
        ports: StreamPorts,
    },
    RecordingStarted,
    ParametersSet(ParameterPacket),
    CommandReceived(Vec<u8>),
    StreamMessage {
        stream_id: u64,
        payload: Vec<u8>,
    },
    ParametersRequested(Vec<String>),
    Flushed(FlushPoint),
    KeepAlive,
    StreamsTornDown(Vec<StreamKey>),
    SessionTornDown,
    Reset {
        reason: String,
        streams: Vec<StreamKey>,
    },
}

#[derive(Debug)]
pub struct ReceiverOutput {
    pub response: RtspResponse,
    pub events: Vec<ReceiverEvent>,
    pub control_keys: Option<ReceiverEncryptionKeys>,
}

pub struct ReceiverEncryptionKeys {
    pub control: ControlKeys,
    pub event: EventChannelKeys,
}

impl std::fmt::Debug for ReceiverEncryptionKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReceiverEncryptionKeys")
            .field("control", &self.control)
            .field("event", &self.event)
            .finish()
    }
}

#[derive(Debug, Clone)]
struct NegotiatedStream {
    descriptor: StreamDescriptor,
}

pub struct AirPlayReceiver<S: MfiSigner> {
    pair_setup: PairSetup,
    pair_verify: PairVerify,
    auth_setup: AuthSetupHandler<S>,
    verification: Option<PairVerification>,
    state: ReceiverState,
    session_id: String,
    info: InfoPayload,
    enabled_features: Vec<String>,
    conditional_features: Vec<(String, Dictionary)>,
    negotiated_features: Vec<String>,
    limits: ReceiverLimits,
    recent_cseqs: VecDeque<u64>,
    session_ports: Option<SessionPorts>,
    streams: BTreeMap<StreamKey, NegotiatedStream>,
    audio_feedback: Vec<AudioFeedbackStream>,
    parameters: BTreeMap<String, String>,
}

impl<S: MfiSigner> std::fmt::Debug for AirPlayReceiver<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AirPlayReceiver")
            .field("pair_setup", &self.pair_setup.state())
            .field("pair_verify", &self.pair_verify.state())
            .field("auth_setup", &self.auth_setup.state())
            .field("verification", &self.verification)
            .field("state", &self.state)
            .field("session_id", &self.session_id)
            .field("recent_cseqs", &self.recent_cseqs.len())
            .field("session_ports", &self.session_ports)
            .field("streams", &self.streams)
            .finish_non_exhaustive()
    }
}

impl<S: MfiSigner> AirPlayReceiver<S> {
    pub fn new(
        config: ReceiverConfig,
        store: Arc<PairingStore>,
        identity: Arc<AccessoryIdentity>,
        signer: S,
    ) -> Result<Self> {
        let limits = config.limits.validate()?;
        plist_codec::decode_dictionary(config.info.as_bytes(), limits.plist)?;
        validate_token("session identifier", &config.session_id, 128)?;
        if config.enabled_features.len() + config.conditional_features.len() > 32
            || config
                .enabled_features
                .iter()
                .chain(
                    config
                        .conditional_features
                        .iter()
                        .map(|(feature, _)| feature),
                )
                .any(|feature| validate_token("enabled feature", feature, 128).is_err())
        {
            return Err(Error::InvalidRtsp(
                "enabled feature list is invalid".to_owned(),
            ));
        }
        let pair_setup = PairSetup::new(
            Arc::clone(&store),
            Arc::clone(&identity),
            config.setup_code,
            config.pairing_timeout,
        )?;
        let pair_verify = PairVerify::new(store, identity, config.pairing_timeout)?;
        Ok(Self {
            pair_setup,
            pair_verify,
            auth_setup: AuthSetupHandler::new(signer),
            verification: None,
            state: ReceiverState::Cleartext,
            session_id: config.session_id,
            info: config.info,
            enabled_features: config.enabled_features,
            conditional_features: config.conditional_features,
            negotiated_features: Vec::new(),
            limits,
            recent_cseqs: VecDeque::with_capacity(RECENT_CSEQ_WINDOW),
            session_ports: None,
            streams: BTreeMap::new(),
            audio_feedback: Vec::new(),
            parameters: BTreeMap::new(),
        })
    }

    pub fn set_audio_feedback(&mut self, streams: Vec<AudioFeedbackStream>) -> Result<()> {
        if streams.len() > self.limits.maximum_streams {
            return Err(Error::MessageTooLarge {
                section: "audio feedback streams",
                actual: streams.len(),
                maximum: self.limits.maximum_streams,
            });
        }
        for stream in &streams {
            stream.validate()?;
        }
        self.audio_feedback = streams;
        Ok(())
    }

    pub fn state(&self) -> ReceiverState {
        self.state
    }

    pub fn negotiated_features(&self) -> &[String] {
        &self.negotiated_features
    }

    pub fn pair_verify_failure_reason(&self) -> Option<&'static str> {
        self.pair_verify.failure_reason()
    }

    pub fn controller_id(&self) -> Option<&str> {
        self.verification
            .as_ref()
            .map(PairVerification::controller_id)
    }

    pub fn active_streams(&self) -> impl ExactSizeIterator<Item = &StreamDescriptor> {
        self.streams.values().map(|stream| &stream.descriptor)
    }

    pub fn active_data_stream_key(
        &self,
        stream: StreamKey,
        direction: DataStreamDirection,
    ) -> Result<Zeroizing<[u8; 32]>> {
        if !self.streams.contains_key(&stream) {
            return Err(Error::NotReady {
                component: "AirPlay data stream",
                state: "stream_not_negotiated",
            });
        }
        self.verification
            .as_ref()
            .ok_or(Error::NotReady {
                component: "AirPlay data stream",
                state: "pair_verify_incomplete",
            })?
            .data_stream_key(stream.connection_id, direction)
    }

    pub fn handle<P: PortProvider>(
        &mut self,
        request: &RtspRequest,
        transport: ControlTransport,
        ports: &mut P,
    ) -> Result<ReceiverOutput> {
        if matches!(self.state, ReceiverState::Failed | ReceiverState::Teardown) {
            return Ok(self.reject(
                454,
                "Session Not Found",
                "request arrived after terminal teardown",
            ));
        }
        if let Some(output) = self.validate_request(request, transport) {
            return Ok(output);
        }
        let path = match request_path(&request.target) {
            Ok(path) => path,
            Err(_) => {
                return Ok(self.reject(400, "Bad Request", "invalid RTSP request target"));
            }
        };
        let result = match (request.method.as_str(), path.as_str()) {
            ("OPTIONS", "*") => Ok(self.options()),
            ("POST", "/pair-setup") => self.pair_setup(request),
            ("POST", "/pair-verify") => self.pair_verify(request),
            ("POST", "/auth-setup") => self.auth_setup(request),
            ("POST", "/command") => Ok(self.command(request)),
            ("POST", "/feedback") => Ok(self.feedback(request)),
            ("GET", "/info") => self.info(request),
            ("SETUP", path) if path.starts_with('/') => self.setup(request, ports),
            ("RECORD", path) if path.starts_with('/') => Ok(self.record(request)),
            ("SET_PARAMETER", path) if path.starts_with('/') => Ok(self.set_parameter(request)),
            ("GET_PARAMETER", path) if path.starts_with('/') => Ok(self.get_parameter(request)),
            ("FLUSH", path) if path.starts_with('/') => Ok(self.flush(request)),
            ("TEARDOWN", path) if path.starts_with('/') => Ok(self.teardown(request)),
            _ => Ok(self.reject(404, "Not Found", "unsupported RTSP method or target")),
        };
        if result.is_err() {
            self.fail_events("receiver operation failed");
        }
        result
    }

    fn command(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if !matches!(
            self.state,
            ReceiverState::Authenticated
                | ReceiverState::SessionConfigured
                | ReceiverState::StreamsConfigured
                | ReceiverState::Recording
        ) {
            return self.reject(
                455,
                "Method Not Valid in This State",
                "command arrived before MFi authentication",
            );
        }
        if !content_type_is(request, PLIST_CONTENT_TYPE) || request.body.is_empty() {
            return self.reject(400, "Bad Request", "invalid command plist content type");
        }
        let Ok(dictionary) = plist_codec::decode_dictionary(&request.body, self.limits.plist)
        else {
            return self.reject(400, "Bad Request", "malformed command plist");
        };
        let event = if dictionary.contains_key("type") {
            ReceiverEvent::CommandReceived(request.body.clone())
        } else {
            match self.stream_message(request, &dictionary) {
                Ok(event) => event,
                Err(reason) => return self.reject(400, "Bad Request", reason),
            }
        };
        ReceiverOutput {
            response: ok_response(),
            events: vec![event],
            control_keys: None,
        }
    }

    fn stream_message(
        &self,
        request: &RtspRequest,
        dictionary: &Dictionary,
    ) -> std::result::Result<ReceiverEvent, &'static str> {
        let stream_id = request
            .headers
            .get("x-apple-streamid")
            .ok_or("untyped command carried no stream identifier")?
            .trim()
            .parse::<u64>()
            .map_err(|_| "command stream identifier is not an integer")?;
        let negotiated = self.streams.values().any(|stream| {
            matches!(
                &stream.descriptor,
                StreamDescriptor::MessageChannel(channel) if channel.stream_id == stream_id
            )
        });
        if !negotiated {
            return Err("command names a stream that was not negotiated");
        }
        let payload = dictionary
            .get("params")
            .and_then(Value::as_dictionary)
            .and_then(|params| params.get("data"))
            .and_then(Value::as_data)
            .ok_or("stream command carried no data")?;
        Ok(ReceiverEvent::StreamMessage {
            stream_id,
            payload: payload.to_vec(),
        })
    }

    fn feedback(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if !matches!(
            self.state,
            ReceiverState::SessionConfigured
                | ReceiverState::StreamsConfigured
                | ReceiverState::Recording
        ) {
            return self.reject(
                455,
                "Method Not Valid in This State",
                "feedback arrived before session SETUP",
            );
        }
        if let Some(output) = self.require_session(request) {
            return output;
        }
        if !request.body.is_empty() {
            return self.reject(400, "Bad Request", "feedback body must be empty");
        }
        let response = if self.audio_feedback.is_empty() {
            ok_response()
        } else {
            let streams = self
                .audio_feedback
                .iter()
                .map(|stream| {
                    let mut value = Dictionary::new();
                    value.insert(
                        "type".to_owned(),
                        Value::Integer(u64::from(stream.stream_type).into()),
                    );
                    value.insert(
                        "sampleRate".to_owned(),
                        Value::Integer(u64::from(stream.sample_rate_hz).into()),
                    );
                    if let Some(anchor) = stream.anchor {
                        value.insert(
                            "streamConnectionID".to_owned(),
                            Value::Integer(anchor.connection_id.into()),
                        );
                        value.insert(
                            "timestamp".to_owned(),
                            Value::Integer(anchor.synchronized_ntp.into()),
                        );
                        value.insert(
                            "timestampRawNs".to_owned(),
                            Value::Integer(anchor.monotonic_nanoseconds.into()),
                        );
                        value.insert(
                            "sampleTime".to_owned(),
                            Value::Integer(u64::from(anchor.sample_time).into()),
                        );
                    }
                    Value::Dictionary(value)
                })
                .collect();
            let mut body = Dictionary::new();
            body.insert("streams".to_owned(), Value::Array(streams));
            match plist_codec::encode(&Value::Dictionary(body), self.limits.plist) {
                Ok(body) => plist_response(body),
                Err(error) => {
                    return self.reject(500, "Internal Server Error", error.to_string());
                }
            }
        };
        ReceiverOutput {
            response,
            events: vec![ReceiverEvent::KeepAlive],
            control_keys: None,
        }
    }

    fn validate_request(
        &mut self,
        request: &RtspRequest,
        transport: ControlTransport,
    ) -> Option<ReceiverOutput> {
        if request.version != "RTSP/1.0" {
            return Some(self.reject(400, "Bad Request", "unsupported RTSP version"));
        }
        if request.headers.len() > self.limits.maximum_headers {
            return Some(self.reject(400, "Bad Request", "too many RTSP headers"));
        }
        if request.body.len() > self.limits.maximum_body_bytes {
            return Some(self.reject(400, "Bad Request", "RTSP body is too large"));
        }
        let header_bytes = request
            .headers
            .iter()
            .try_fold(0usize, |total, (name, value)| {
                total.checked_add(name.len() + value.len() + 4)
            });
        if header_bytes.is_none_or(|bytes| bytes > self.limits.maximum_header_bytes)
            || request.headers.iter().any(|(name, value)| {
                name.is_empty()
                    || name.bytes().any(|byte| {
                        !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                    })
                    || (name == "x-apple-client-name" && value.chars().any(char::is_control))
                    || (name != "x-apple-client-name" && !value.is_ascii())
                    || value.contains(['\r', '\n'])
            })
        {
            return Some(self.reject(400, "Bad Request", "invalid RTSP headers"));
        }
        match request.headers.get("content-length") {
            Some(value)
                if value
                    .parse::<usize>()
                    .is_ok_and(|length| length == request.body.len()) => {}
            None if request.body.is_empty() => {}
            _ => return Some(self.reject(400, "Bad Request", "Content-Length mismatch")),
        }
        let cseq = match request.headers.get("cseq").and_then(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            value.parse::<u64>().ok()
        }) {
            Some(cseq) => cseq,
            None => return Some(self.reject(400, "Bad Request", "missing or invalid CSeq")),
        };
        if self.recent_cseqs.contains(&cseq) {
            return Some(self.reject(400, "Bad Request", "replayed CSeq"));
        }
        if self.recent_cseqs.len() == RECENT_CSEQ_WINDOW {
            self.recent_cseqs.pop_front();
        }
        self.recent_cseqs.push_back(cseq);

        match (self.state, transport) {
            (ReceiverState::Cleartext, ControlTransport::Cleartext) => {}
            (ReceiverState::AwaitingEncryptedControl, ControlTransport::Encrypted) => {
                self.state = ReceiverState::Encrypted;
            }
            (state, ControlTransport::Encrypted) if state.encrypted() => {}
            _ => {
                return Some(self.reject(
                    455,
                    "Method Not Valid in This State",
                    "control transport encryption state mismatch",
                ));
            }
        }
        None
    }

    fn options(&mut self) -> ReceiverOutput {
        let mut response = ok_response();
        response
            .headers
            .insert("public".to_owned(), PUBLIC_METHODS.to_owned());
        ReceiverOutput {
            response,
            events: vec![ReceiverEvent::KeepAlive],
            control_keys: None,
        }
    }

    fn pair_setup(&mut self, request: &RtspRequest) -> Result<ReceiverOutput> {
        if self.state != ReceiverState::Cleartext {
            return Ok(self.reject(455, "Method Not Valid in This State", "late pair-setup"));
        }
        if !pair_setup_request_content_type_is(request) {
            return Ok(self.reject(400, "Bad Request", "invalid pair-setup content type"));
        }
        let body = self.pair_setup.handle(&request.body)?;
        let mut response = ok_response();
        response
            .headers
            .insert("content-type".to_owned(), PAIRING_CONTENT_TYPE.to_owned());
        response.body = body;
        let events = if self.pair_setup.state() == PairSetupState::Failed {
            self.fail_events("pair-setup authentication failed")
        } else {
            if self.pair_setup.state() == PairSetupState::Complete
                && self.pair_verify.state() == PairVerifyState::Failed
            {
                self.pair_verify.restart()?;
            }
            Vec::new()
        };
        Ok(ReceiverOutput {
            response,
            events,
            control_keys: None,
        })
    }

    fn pair_verify(&mut self, request: &RtspRequest) -> Result<ReceiverOutput> {
        if self.state != ReceiverState::Cleartext {
            return Ok(self.reject(455, "Method Not Valid in This State", "late pair-verify"));
        }
        if !pair_verify_request_content_type_is(request) {
            return Ok(self.reject(400, "Bad Request", "invalid pair-verify content type"));
        }
        let body = self.pair_verify.handle(&request.body)?;
        let mut response = ok_response();
        response
            .headers
            .insert("content-type".to_owned(), PAIRING_CONTENT_TYPE.to_owned());
        response.body = body;
        if self.pair_verify.state() == PairVerifyState::Failed {
            let events = if self.pair_setup.state() == PairSetupState::AwaitingM1 {
                Vec::new()
            } else {
                self.fail_events("pair-verify authentication failed")
            };
            return Ok(ReceiverOutput {
                response,
                events,
                control_keys: None,
            });
        }
        if self.pair_verify.state() != PairVerifyState::Verified {
            return Ok(ReceiverOutput {
                response,
                events: Vec::new(),
                control_keys: None,
            });
        }
        let verification = self
            .pair_verify
            .take_verification()
            .ok_or(Error::Crypto("verified pair-verify lost its result"))?;
        let keys = ReceiverEncryptionKeys {
            control: verification.control_keys()?,
            event: verification.event_channel_keys()?,
        };
        let controller_id = verification.controller_id().to_owned();
        self.verification = Some(verification);
        self.state = ReceiverState::AwaitingEncryptedControl;
        Ok(ReceiverOutput {
            response,
            events: vec![ReceiverEvent::ControlEncryptionReady { controller_id }],
            control_keys: Some(keys),
        })
    }

    fn auth_setup(&mut self, request: &RtspRequest) -> Result<ReceiverOutput> {
        if self.state != ReceiverState::Encrypted {
            return Ok(self.reject(455, "Method Not Valid in This State", "early auth-setup"));
        }
        if !content_type_is(request, "application/octet-stream") {
            return Ok(self.reject(400, "Bad Request", "invalid auth-setup content type"));
        }
        let verification = self
            .verification
            .as_ref()
            .ok_or(Error::Crypto("encrypted state lost pair verification"))?;
        let response = self.auth_setup.handle(request, verification)?;
        let succeeded = response.status == 200;
        if succeeded {
            self.state = ReceiverState::Authenticated;
        }
        let events = if succeeded {
            vec![ReceiverEvent::MfiAuthenticated]
        } else {
            self.fail_events("MFi authentication failed")
        };
        Ok(ReceiverOutput {
            response,
            events,
            control_keys: None,
        })
    }

    fn info(&mut self, request: &RtspRequest) -> Result<ReceiverOutput> {
        if !matches!(
            self.state,
            ReceiverState::SessionConfigured
                | ReceiverState::StreamsConfigured
                | ReceiverState::Recording
        ) {
            return Ok(self.reject(455, "Method Not Valid in This State", "out-of-order /info"));
        }
        if let Some(output) = self.require_session(request) {
            return Ok(output);
        }
        if !request.body.is_empty() {
            if !content_type_is(request, PLIST_CONTENT_TYPE) {
                return Ok(self.reject(400, "Bad Request", "invalid /info content type"));
            }
            if plist_codec::decode_dictionary(&request.body, self.limits.plist).is_err() {
                return Ok(self.reject(400, "Bad Request", "malformed /info plist"));
            }
        }
        let mut response = ok_response();
        response
            .headers
            .insert("content-type".to_owned(), PLIST_CONTENT_TYPE.to_owned());
        response.body = self.negotiated_info()?;
        Ok(ReceiverOutput {
            response,
            events: vec![ReceiverEvent::InfoRequested],
            control_keys: None,
        })
    }

    fn negotiated_info(&self) -> Result<Vec<u8>> {
        let extensions: Vec<&Dictionary> = self
            .conditional_features
            .iter()
            .filter(|(feature, entries)| {
                !entries.is_empty() && self.negotiated_features.contains(feature)
            })
            .map(|(_, entries)| entries)
            .collect();
        if extensions.is_empty() {
            return Ok(self.info.encoded.clone());
        }
        let mut info = plist_codec::decode_dictionary(&self.info.encoded, self.limits.plist)?;
        for entries in extensions {
            for (key, value) in entries {
                info.insert(key.clone(), value.clone());
            }
        }
        plist_codec::encode(&Value::Dictionary(info), self.limits.plist)
    }

    fn setup<P: PortProvider>(
        &mut self,
        request: &RtspRequest,
        provider: &mut P,
    ) -> Result<ReceiverOutput> {
        if !content_type_is(request, PLIST_CONTENT_TYPE) {
            return Ok(self.reject(400, "Bad Request", "invalid SETUP content type"));
        }
        let dictionary = match plist_codec::decode_dictionary(&request.body, self.limits.plist) {
            Ok(dictionary) => dictionary,
            Err(_) => return Ok(self.reject(400, "Bad Request", "malformed SETUP plist")),
        };
        if dictionary.contains_key("streams") {
            self.setup_streams(request, &dictionary, provider)
        } else {
            self.setup_session(request, &dictionary, provider)
        }
    }

    fn setup_session<P: PortProvider>(
        &mut self,
        request: &RtspRequest,
        dictionary: &Dictionary,
        provider: &mut P,
    ) -> Result<ReceiverOutput> {
        if self.state != ReceiverState::Authenticated {
            return Ok(self.reject(
                455,
                "Method Not Valid in This State",
                "duplicate session SETUP",
            ));
        }
        if request.headers.contains_key("session") {
            return Ok(self.reject(454, "Session Not Found", "session SETUP carried Session"));
        }
        let timing = match parse_timing(dictionary) {
            Ok(timing) => timing,
            Err(_) => return Ok(self.reject(400, "Bad Request", "malformed timing SETUP")),
        };
        let ports = provider.session_ports(&timing)?;
        if timing.low_power_keepalive != ports.keep_alive_port.is_some() {
            return Ok(self.reject(
                500,
                "Internal Server Error",
                "keepalive port did not match the peer's low-power request",
            ));
        }
        let negotiated: Vec<String> = self
            .conditional_features
            .iter()
            .map(|(feature, _)| feature)
            .filter(|feature| timing.offered_features.contains(feature))
            .cloned()
            .collect();
        let enabled: Vec<String> = self
            .enabled_features
            .iter()
            .chain(&negotiated)
            .cloned()
            .collect();
        let response_body = session_setup_response(ports, &enabled, self.limits.plist)?;
        self.negotiated_features = negotiated;
        self.session_ports = Some(ports);
        self.state = ReceiverState::SessionConfigured;
        let response = plist_response(response_body);
        Ok(ReceiverOutput {
            response,
            events: vec![ReceiverEvent::SessionNegotiated { timing, ports }],
            control_keys: None,
        })
    }

    fn setup_streams<P: PortProvider>(
        &mut self,
        request: &RtspRequest,
        dictionary: &Dictionary,
        provider: &mut P,
    ) -> Result<ReceiverOutput> {
        if !matches!(
            self.state,
            ReceiverState::SessionConfigured
                | ReceiverState::StreamsConfigured
                | ReceiverState::Recording
        ) {
            return Ok(self.reject(
                455,
                "Method Not Valid in This State",
                "out-of-order stream SETUP",
            ));
        }
        if let Some(output) = self.require_session(request) {
            return Ok(output);
        }
        let mut descriptors = match parse_streams(dictionary, self.limits.maximum_streams) {
            Ok(descriptors) => descriptors,
            Err(Error::Unsupported(_)) => {
                return Ok(self.reject(
                    461,
                    "Unsupported Transport",
                    format!("unsupported SETUP stream: {}", describe_streams(dictionary)),
                ));
            }
            Err(error) => {
                return Ok(self.reject(
                    400,
                    "Bad Request",
                    format!(
                        "malformed stream SETUP: {error}: {}",
                        describe_streams(dictionary)
                    ),
                ));
            }
        };
        let open_data_channels = self
            .streams
            .values()
            .filter(|stream| stream.descriptor.stream_type() == DATA_CHANNEL_STREAM_TYPE)
            .count();
        let mut next_stream_id = open_data_channels as u64 + 1;
        for descriptor in &mut descriptors {
            match descriptor {
                StreamDescriptor::MessageChannel(channel) => {
                    channel.stream_id = next_stream_id;
                    next_stream_id += 1;
                }
                StreamDescriptor::DataChannel(_) => next_stream_id += 1,
                StreamDescriptor::Screen(_) | StreamDescriptor::Audio(_) => {}
            }
        }
        let mut keys = BTreeSet::new();
        for descriptor in &descriptors {
            if !keys.insert(descriptor.key()) || self.streams.contains_key(&descriptor.key()) {
                return Ok(self.reject(400, "Bad Request", "duplicate stream SETUP"));
            }
        }
        if self.streams.len() + descriptors.len() > self.limits.maximum_streams {
            return Ok(self.reject(400, "Bad Request", "too many negotiated streams"));
        }

        let mut negotiated = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            let ports = provider.stream_ports(&descriptor)?;
            if !ports.matches(&descriptor) {
                return Ok(self.reject(
                    500,
                    "Internal Server Error",
                    "port type did not match stream",
                ));
            }
            negotiated.push((descriptor, ports));
        }
        let response_body =
            streams_setup_response(&negotiated, open_data_channels, self.limits.plist)?;
        let mut events = Vec::with_capacity(negotiated.len());
        for (descriptor, ports) in negotiated {
            events.push(ReceiverEvent::StreamNegotiated {
                stream: descriptor.clone(),
                ports,
            });
            self.streams
                .insert(descriptor.key(), NegotiatedStream { descriptor });
        }
        if self.state != ReceiverState::Recording {
            self.state = ReceiverState::StreamsConfigured;
        }
        let response = plist_response(response_body);
        Ok(ReceiverOutput {
            response,
            events,
            control_keys: None,
        })
    }

    fn record(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if !matches!(
            self.state,
            ReceiverState::SessionConfigured | ReceiverState::StreamsConfigured
        ) {
            return self.reject(
                455,
                "Method Not Valid in This State",
                "RECORD before session SETUP or duplicate RECORD",
            );
        }
        if let Some(output) = self.require_session(request) {
            return output;
        }
        if !request.body.is_empty() {
            return self.reject(400, "Bad Request", "RECORD body must be empty");
        }
        self.state = ReceiverState::Recording;
        let response = ok_response();
        ReceiverOutput {
            response,
            events: vec![ReceiverEvent::RecordingStarted],
            control_keys: None,
        }
    }

    fn set_parameter(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if self.state != ReceiverState::Recording {
            return self.reject(
                455,
                "Method Not Valid in This State",
                "SET_PARAMETER before RECORD",
            );
        }
        if let Some(output) = self.require_session(request) {
            return output;
        }
        let content_type = request.headers.get("content-type").cloned();
        let text_values = if content_type.as_deref().is_some_and(|value| {
            media_type(value).eq_ignore_ascii_case(TEXT_PARAMETERS_CONTENT_TYPE)
        }) {
            match parse_parameter_values(&request.body, self.limits) {
                Ok(values) => values,
                Err(_) => return self.reject(400, "Bad Request", "malformed text parameters"),
            }
        } else {
            if content_type
                .as_deref()
                .is_some_and(|value| media_type(value).eq_ignore_ascii_case(PLIST_CONTENT_TYPE))
                && plist_codec::decode(&request.body, self.limits.plist).is_err()
            {
                return self.reject(400, "Bad Request", "malformed parameter plist");
            }
            BTreeMap::new()
        };
        self.parameters.extend(text_values.clone());
        let packet = ParameterPacket {
            content_type,
            body: request.body.clone(),
            text_values,
        };
        let response = ok_response();
        ReceiverOutput {
            response,
            events: vec![ReceiverEvent::ParametersSet(packet)],
            control_keys: None,
        }
    }

    fn get_parameter(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if self.state != ReceiverState::Recording {
            return self.reject(
                455,
                "Method Not Valid in This State",
                "GET_PARAMETER before RECORD",
            );
        }
        if let Some(output) = self.require_session(request) {
            return output;
        }
        let names = match parse_parameter_names(&request.body, self.limits) {
            Ok(names) => names,
            Err(_) => return self.reject(400, "Bad Request", "malformed parameter query"),
        };
        let mut body = Vec::new();
        for name in &names {
            if let Some(value) = self.parameters.get(name) {
                body.extend_from_slice(name.as_bytes());
                body.extend_from_slice(b": ");
                body.extend_from_slice(value.as_bytes());
                body.extend_from_slice(b"\r\n");
            }
        }
        let mut response = ok_response();
        response.headers.insert(
            "content-type".to_owned(),
            TEXT_PARAMETERS_CONTENT_TYPE.to_owned(),
        );
        response.body = body;
        ReceiverOutput {
            response,
            events: vec![
                ReceiverEvent::ParametersRequested(names),
                ReceiverEvent::KeepAlive,
            ],
            control_keys: None,
        }
    }

    fn flush(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if self.state != ReceiverState::Recording {
            return self.reject(455, "Method Not Valid in This State", "FLUSH before RECORD");
        }
        if let Some(output) = self.require_session(request) {
            return output;
        }
        if !request.body.is_empty() {
            return self.reject(400, "Bad Request", "FLUSH body must be empty");
        }
        let point = match request.headers.get("rtp-info") {
            Some(header) => match parse_rtp_info(header) {
                Ok(point) => point,
                Err(_) => return self.reject(400, "Bad Request", "malformed RTP-Info"),
            },
            None => FlushPoint {
                sequence: None,
                rtp_time: None,
            },
        };
        let response = ok_response();
        ReceiverOutput {
            response,
            events: vec![ReceiverEvent::Flushed(point)],
            control_keys: None,
        }
    }

    fn teardown(&mut self, request: &RtspRequest) -> ReceiverOutput {
        if !matches!(
            self.state,
            ReceiverState::SessionConfigured
                | ReceiverState::StreamsConfigured
                | ReceiverState::Recording
        ) {
            return self.reject(
                455,
                "Method Not Valid in This State",
                "TEARDOWN before SETUP",
            );
        }
        if let Some(output) = self.require_session(request) {
            return output;
        }
        if request.body.is_empty() {
            return self.teardown_session();
        }
        if !content_type_is(request, PLIST_CONTENT_TYPE) {
            return self.reject(400, "Bad Request", "invalid TEARDOWN content type");
        }
        let dictionary = match plist_codec::decode_dictionary(&request.body, self.limits.plist) {
            Ok(dictionary) => dictionary,
            Err(_) => return self.reject(400, "Bad Request", "malformed TEARDOWN plist"),
        };
        if dictionary.is_empty() {
            return self.teardown_session();
        }
        let selectors = match parse_teardown_selectors(&dictionary, self.limits.maximum_streams) {
            Ok(selectors) => selectors,
            Err(_) => return self.reject(400, "Bad Request", "malformed TEARDOWN streams"),
        };
        let mut removed = Vec::new();
        self.streams.retain(|key, _| {
            let remove = selectors.iter().any(|selector| selector.matches(*key));
            if remove {
                removed.push(*key);
            }
            !remove
        });
        if removed.is_empty() {
            return self.reject(454, "Session Not Found", "TEARDOWN named no active stream");
        }
        self.state = if self.state == ReceiverState::Recording {
            ReceiverState::Recording
        } else if self.streams.is_empty() {
            ReceiverState::SessionConfigured
        } else {
            ReceiverState::StreamsConfigured
        };
        let response = ok_response();
        ReceiverOutput {
            response,
            events: vec![ReceiverEvent::StreamsTornDown(removed)],
            control_keys: None,
        }
    }

    fn teardown_session(&mut self) -> ReceiverOutput {
        self.streams.clear();
        self.session_ports = None;
        self.verification = None;
        self.state = ReceiverState::Teardown;
        ReceiverOutput {
            response: ok_response(),
            events: vec![ReceiverEvent::SessionTornDown],
            control_keys: None,
        }
    }

    fn require_session(&mut self, request: &RtspRequest) -> Option<ReceiverOutput> {
        let provided = request.headers.get("session").map(String::as_str);
        match provided {
            None => None,
            Some(provided) if provided == self.session_id => None,
            Some(_) => Some(self.reject(454, "Session Not Found", "mismatched Session header")),
        }
    }

    fn reject(
        &mut self,
        status: u16,
        reason: &'static str,
        detail: impl Into<String>,
    ) -> ReceiverOutput {
        let detail = detail.into();
        let events = self.fail_events(detail);
        ReceiverOutput {
            response: RtspResponse::new(status, reason),
            events,
            control_keys: None,
        }
    }

    fn fail_events(&mut self, reason: impl Into<String>) -> Vec<ReceiverEvent> {
        let streams = self.streams.keys().copied().collect();
        self.streams.clear();
        self.session_ports = None;
        self.verification = None;
        self.state = ReceiverState::Failed;
        vec![ReceiverEvent::Reset {
            reason: reason.into(),
            streams,
        }]
    }
}

fn ok_response() -> RtspResponse {
    RtspResponse::new(200, "OK")
}

fn plist_response(body: Vec<u8>) -> RtspResponse {
    let mut response = ok_response();
    response
        .headers
        .insert("content-type".to_owned(), PLIST_CONTENT_TYPE.to_owned());
    response.body = body;
    response
}

fn content_type_is(request: &RtspRequest, expected: &str) -> bool {
    request
        .headers
        .get("content-type")
        .is_some_and(|value| media_type(value).eq_ignore_ascii_case(expected))
}

fn pair_setup_request_content_type_is(request: &RtspRequest) -> bool {
    content_type_is(request, PAIRING_CONTENT_TYPE)
        || content_type_is(request, CARPLAY_PAIRING_REQUEST_CONTENT_TYPE)
}

fn pair_verify_request_content_type_is(request: &RtspRequest) -> bool {
    content_type_is(request, PAIRING_CONTENT_TYPE)
        || content_type_is(request, CARPLAY_VERIFY_REQUEST_CONTENT_TYPE)
}

fn media_type(value: &str) -> &str {
    value.split(';').next().unwrap_or_default().trim()
}

fn request_path(target: &str) -> Result<String> {
    if target == "*" {
        return Ok("*".to_owned());
    }
    let path = if target.starts_with('/') {
        target
    } else if let Some(authority_and_path) = target.strip_prefix("rtsp://") {
        let path_start = authority_and_path.find('/');
        let authority = path_start.map_or(authority_and_path, |index| &authority_and_path[..index]);
        if authority.is_empty() || authority.contains(['@', '\r', '\n']) {
            return Err(Error::InvalidRtsp(
                "invalid RTSP target authority".to_owned(),
            ));
        }
        path_start.map_or("/", |index| &authority_and_path[index..])
    } else {
        return Err(Error::InvalidRtsp("invalid RTSP target".to_owned()));
    };
    if path.contains(['?', '#', '\r', '\n']) || !path.is_ascii() {
        return Err(Error::InvalidRtsp("invalid RTSP target path".to_owned()));
    }
    Ok(path.to_owned())
}

fn validate_token(label: &'static str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b';')
    {
        return Err(Error::InvalidRtsp(format!("invalid {label}")));
    }
    Ok(())
}

fn optional_string(dictionary: &Dictionary, key: &str, maximum: usize) -> Result<Option<String>> {
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    let string = value
        .as_string()
        .ok_or_else(|| Error::InvalidRtsp(format!("plist field {key} is not a string")))?;
    if string.len() > maximum || string.chars().any(char::is_control) {
        return Err(Error::InvalidRtsp(format!("plist field {key} is invalid")));
    }
    Ok(Some(string.to_owned()))
}

fn optional_u64(dictionary: &Dictionary, key: &str) -> Result<Option<u64>> {
    dictionary
        .get(key)
        .map(|value| {
            value
                .as_unsigned_integer()
                .ok_or_else(|| Error::InvalidRtsp(format!("plist field {key} is not unsigned")))
        })
        .transpose()
}

fn optional_u64_identifier(dictionary: &Dictionary, key: &str) -> Result<Option<u64>> {
    dictionary
        .get(key)
        .map(|value| match value {
            Value::Integer(value) => value
                .as_unsigned()
                .or_else(|| value.as_signed().map(|signed| signed as u64))
                .ok_or_else(|| {
                    Error::InvalidRtsp(format!("plist field {key} is not a 64-bit integer"))
                }),
            _ => Err(Error::InvalidRtsp(format!(
                "plist field {key} is not an integer"
            ))),
        })
        .transpose()
}

fn optional_u32(dictionary: &Dictionary, key: &str) -> Result<Option<u32>> {
    optional_u64(dictionary, key)?
        .map(|value| {
            value
                .try_into()
                .map_err(|_| Error::InvalidRtsp(format!("plist field {key} exceeds u32")))
        })
        .transpose()
}

fn optional_port(dictionary: &Dictionary, key: &str) -> Result<Option<NonZeroU16>> {
    let Some(port) = optional_u64(dictionary, key)? else {
        return Ok(None);
    };
    if port == 0 {
        return Ok(None);
    }
    u16::try_from(port)
        .ok()
        .and_then(NonZeroU16::new)
        .map(Some)
        .ok_or_else(|| Error::InvalidRtsp(format!("plist field {key} is not a port")))
}

fn parse_timing(dictionary: &Dictionary) -> Result<TimingDescriptor> {
    let low_power_keepalive = match dictionary.get("keepAliveLowPower") {
        Some(Value::Boolean(value)) => *value,
        None => false,
        Some(_) => {
            return Err(Error::InvalidRtsp(
                "plist field keepAliveLowPower is not Boolean".to_owned(),
            ));
        }
    };
    Ok(TimingDescriptor {
        peer_timing_port: optional_port(dictionary, "timingPort")?,
        low_power_keepalive,
        peer: PeerIdentity {
            name: optional_string(dictionary, "name", 256)?,
            device_id: optional_string(dictionary, "deviceID", 256)?,
            wifi_mac_address: optional_string(dictionary, "macAddress", 64)?,
            model: optional_string(dictionary, "model", 256)?,
        },
        offered_features: parse_offered_features(dictionary)?,
    })
}

fn parse_offered_features(dictionary: &Dictionary) -> Result<Vec<String>> {
    let Some(features) = dictionary.get("features") else {
        return Ok(Vec::new());
    };
    let features = features
        .as_array()
        .ok_or_else(|| Error::InvalidRtsp("plist field features is not an array".to_owned()))?;
    if features.len() > 64 {
        return Err(Error::InvalidRtsp(
            "session SETUP offers too many features".to_owned(),
        ));
    }
    Ok(features
        .iter()
        .filter_map(Value::as_string)
        .filter(|feature| feature.len() <= 128)
        .map(str::to_owned)
        .collect())
}

fn parse_streams(dictionary: &Dictionary, maximum: usize) -> Result<Vec<StreamDescriptor>> {
    let streams = dictionary
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidRtsp("SETUP streams is not an array".to_owned()))?;
    if streams.is_empty() || streams.len() > maximum {
        return Err(Error::InvalidRtsp(
            "SETUP streams is empty or too large".to_owned(),
        ));
    }
    streams
        .iter()
        .map(|value| {
            value
                .as_dictionary()
                .ok_or_else(|| Error::InvalidRtsp("SETUP stream is not a dictionary".to_owned()))
                .and_then(parse_stream)
        })
        .collect()
}

fn describe_streams(dictionary: &Dictionary) -> String {
    let Some(streams) = dictionary.get("streams").and_then(Value::as_array) else {
        return "no streams array".to_owned();
    };
    let described: Vec<String> = streams
        .iter()
        .map(|stream| match stream.as_dictionary() {
            Some(stream) => {
                let stream_type = stream
                    .get("type")
                    .and_then(Value::as_unsigned_integer)
                    .map_or_else(|| "?".to_owned(), |value| value.to_string());
                let fields: Vec<&str> = stream.keys().map(String::as_str).collect();
                format!("type {stream_type} [{}]", fields.join(","))
            }
            None => "non-dictionary".to_owned(),
        })
        .collect();
    described.join("; ")
}

fn parse_stream(dictionary: &Dictionary) -> Result<StreamDescriptor> {
    let stream_type = optional_u64(dictionary, "type")?
        .ok_or_else(|| Error::InvalidRtsp("SETUP stream has no type".to_owned()))?;
    let stream_type: u16 = stream_type
        .try_into()
        .map_err(|_| Error::InvalidRtsp("SETUP stream type exceeds u16".to_owned()))?;
    if stream_type == DATA_CHANNEL_STREAM_TYPE {
        let client_type_uuid = optional_string(dictionary, "clientTypeUUID", 64)?;
        if client_type_uuid
            .as_deref()
            .is_some_and(|uuid| uuid.eq_ignore_ascii_case(VIDEO_PLAYBACK_CLIENT_TYPE))
        {
            return Ok(StreamDescriptor::MessageChannel(MessageChannelDescriptor {
                stream_id: 0,
                client_type_uuid: VIDEO_PLAYBACK_CLIENT_TYPE.to_owned(),
                client_uuid: optional_string(dictionary, "clientUUID", 128)?,
            }));
        }
        let seed = optional_u64_identifier(dictionary, "seed")?
            .ok_or_else(|| Error::InvalidRtsp("data channel SETUP has no seed".to_owned()))?;
        return Ok(StreamDescriptor::DataChannel(DataChannelDescriptor {
            seed,
            channel_id: optional_string(dictionary, "channelID", 64)?,
            client_type_uuid,
        }));
    }
    let connection_id = optional_u64_identifier(dictionary, "streamConnectionID")?
        .ok_or_else(|| Error::InvalidRtsp("SETUP stream has no connection ID".to_owned()))?;
    match stream_type {
        MAIN_SCREEN_STREAM_TYPE | ALTERNATE_SCREEN_STREAM_TYPE => {
            Ok(StreamDescriptor::Screen(ScreenDescriptor {
                kind: if stream_type == MAIN_SCREEN_STREAM_TYPE {
                    ScreenKind::Main
                } else {
                    ScreenKind::Alternate
                },
                connection_id,
            }))
        }
        MAIN_AUDIO_STREAM_TYPE | ALTERNATE_AUDIO_STREAM_TYPE | HIGH_AUDIO_STREAM_TYPE => {
            let audio_type =
                optional_string(dictionary, "audioType", 64)?.unwrap_or_else(|| "media".to_owned());
            let audio_format = optional_u64(dictionary, "audioFormat")?.unwrap_or(0);
            let frames_per_packet = optional_u32(dictionary, "framesPerPacket")?
                .and_then(std::num::NonZeroU32::new)
                .map(std::num::NonZeroU32::get);
            let audio_latency_ms = optional_u32(dictionary, "audioLatencyMs")?;
            if audio_latency_ms.is_some_and(|latency| latency > 60_000) {
                return Err(Error::InvalidRtsp(
                    "audioLatencyMs exceeds 60 seconds".to_owned(),
                ));
            }
            let peer_data_port = optional_port(dictionary, "dataPort")?;
            if peer_data_port.is_some() && stream_type != MAIN_AUDIO_STREAM_TYPE {
                return Err(Error::InvalidRtsp(
                    "microphone uplink is only valid on MainAudio".to_owned(),
                ));
            }
            Ok(StreamDescriptor::Audio(AudioDescriptor {
                kind: match stream_type {
                    MAIN_AUDIO_STREAM_TYPE => AudioStreamKind::Main,
                    ALTERNATE_AUDIO_STREAM_TYPE => AudioStreamKind::Alternate,
                    _ => AudioStreamKind::High,
                },
                connection_id,
                audio_type,
                audio_format,
                audio_latency_ms,
                frames_per_packet,
                microphone: peer_data_port.map(|peer_data_port| MicrophoneDescriptor {
                    peer_data_port,
                    frames_per_packet,
                    audio_format,
                }),
            }))
        }
        _ => Err(Error::Unsupported("unsupported CarPlay SETUP stream type")),
    }
}

fn session_setup_response(
    ports: SessionPorts,
    features: &[String],
    limits: PlistLimits,
) -> Result<Vec<u8>> {
    let mut dictionary = Dictionary::new();
    dictionary.insert(
        "timingPort".to_owned(),
        Value::Integer(u64::from(ports.timing_port.get()).into()),
    );
    dictionary.insert(
        "eventPort".to_owned(),
        Value::Integer(u64::from(ports.event_port.get()).into()),
    );
    if let Some(port) = ports.keep_alive_port {
        dictionary.insert(
            "keepAlivePort".to_owned(),
            Value::Integer(u64::from(port.get()).into()),
        );
    }
    if !features.is_empty() {
        dictionary.insert(
            "enabledFeatures".to_owned(),
            Value::Array(features.iter().cloned().map(Value::String).collect()),
        );
    }
    plist_codec::encode(&Value::Dictionary(dictionary), limits)
}

fn streams_setup_response(
    negotiated: &[(StreamDescriptor, StreamPorts)],
    open_data_channels: usize,
    limits: PlistLimits,
) -> Result<Vec<u8>> {
    let mut next_data_stream_id = open_data_channels as u64 + 1;
    let streams = negotiated
        .iter()
        .map(|(descriptor, ports)| {
            let mut dictionary = Dictionary::new();
            dictionary.insert(
                "type".to_owned(),
                Value::Integer(u64::from(descriptor.stream_type()).into()),
            );
            match ports {
                StreamPorts::Screen { data_port } => {
                    dictionary.insert(
                        "dataPort".to_owned(),
                        Value::Integer(u64::from(data_port.get()).into()),
                    );
                }
                StreamPorts::DataChannel { data_port } => {
                    dictionary.insert(
                        "streamID".to_owned(),
                        Value::Integer(next_data_stream_id.into()),
                    );
                    next_data_stream_id += 1;
                    dictionary.insert(
                        "dataPort".to_owned(),
                        Value::Integer(u64::from(data_port.get()).into()),
                    );
                }
                StreamPorts::MessageChannel => {
                    dictionary.insert(
                        "streamID".to_owned(),
                        Value::Integer(descriptor.connection_id().into()),
                    );
                    next_data_stream_id += 1;
                }
                StreamPorts::Audio {
                    data_port,
                    control_port,
                } => {
                    dictionary.insert(
                        "dataPort".to_owned(),
                        Value::Integer(u64::from(data_port.get()).into()),
                    );
                    dictionary.insert(
                        "controlPort".to_owned(),
                        Value::Integer(u64::from(control_port.get()).into()),
                    );
                    dictionary.insert(
                        "streamConnectionID".to_owned(),
                        Value::Integer(descriptor.connection_id().into()),
                    );
                }
            }
            Value::Dictionary(dictionary)
        })
        .collect();
    let mut response = Dictionary::new();
    response.insert("streams".to_owned(), Value::Array(streams));
    plist_codec::encode(&Value::Dictionary(response), limits)
}

fn parse_parameter_values(body: &[u8], limits: ReceiverLimits) -> Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(body)
        .map_err(|_| Error::InvalidRtsp("text parameters are not UTF-8".to_owned()))?;
    let mut values = BTreeMap::new();
    for line in text.split("\r\n").filter(|line| !line.is_empty()) {
        if values.len() >= limits.maximum_parameter_fields {
            return Err(Error::InvalidRtsp("too many text parameters".to_owned()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::InvalidRtsp("text parameter has no colon".to_owned()))?;
        let name = name.trim();
        let value = value.trim();
        validate_parameter(name, value, limits)?;
        if values.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(Error::InvalidRtsp("duplicate text parameter".to_owned()));
        }
    }
    if values.is_empty() {
        return Err(Error::InvalidRtsp("empty text parameters".to_owned()));
    }
    Ok(values)
}

fn parse_parameter_names(body: &[u8], limits: ReceiverLimits) -> Result<Vec<String>> {
    let text = std::str::from_utf8(body)
        .map_err(|_| Error::InvalidRtsp("parameter query is not UTF-8".to_owned()))?;
    let mut names = Vec::new();
    let mut unique = BTreeSet::new();
    for line in text.split("\r\n").filter(|line| !line.is_empty()) {
        let name = line.trim();
        validate_parameter(name, "", limits)?;
        if !unique.insert(name.to_owned()) {
            return Err(Error::InvalidRtsp("duplicate parameter query".to_owned()));
        }
        names.push(name.to_owned());
        if names.len() > limits.maximum_parameter_fields {
            return Err(Error::InvalidRtsp("too many parameter queries".to_owned()));
        }
    }
    Ok(names)
}

fn validate_parameter(name: &str, value: &str, limits: ReceiverLimits) -> Result<()> {
    if name.is_empty()
        || name.len() > limits.maximum_parameter_name_bytes
        || value.len() > limits.maximum_parameter_value_bytes
        || !name.is_ascii()
        || !value.is_ascii()
        || name
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b':')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(Error::InvalidRtsp("invalid text parameter".to_owned()));
    }
    Ok(())
}

fn parse_rtp_info(header: &str) -> Result<FlushPoint> {
    let mut sequence = None;
    let mut rtp_time = None;
    for field in header.split(';') {
        let (name, value) = field
            .trim()
            .split_once('=')
            .ok_or_else(|| Error::InvalidRtsp("malformed RTP-Info field".to_owned()))?;
        match name {
            "seq" if sequence.is_none() => {
                sequence = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| Error::InvalidRtsp("invalid RTP sequence".to_owned()))?,
                );
            }
            "rtptime" if rtp_time.is_none() => {
                rtp_time = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| Error::InvalidRtsp("invalid RTP time".to_owned()))?,
                );
            }
            _ => {
                return Err(Error::InvalidRtsp(
                    "unknown or duplicate RTP-Info field".to_owned(),
                ));
            }
        }
    }
    if sequence.is_none() && rtp_time.is_none() {
        return Err(Error::InvalidRtsp("empty RTP-Info".to_owned()));
    }
    Ok(FlushPoint { sequence, rtp_time })
}

#[derive(Debug, Clone, Copy)]
struct TeardownSelector {
    stream_type: u16,
    connection_id: Option<u64>,
}

impl TeardownSelector {
    const fn matches(self, key: StreamKey) -> bool {
        self.stream_type == key.stream_type
            && match self.connection_id {
                Some(connection_id) => connection_id == key.connection_id,
                None => true,
            }
    }
}

fn parse_teardown_selectors(
    dictionary: &Dictionary,
    maximum: usize,
) -> Result<Vec<TeardownSelector>> {
    let values = dictionary
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidRtsp("TEARDOWN streams is not an array".to_owned()))?;
    if values.is_empty() || values.len() > maximum {
        return Err(Error::InvalidRtsp(
            "TEARDOWN stream list is empty or too large".to_owned(),
        ));
    }
    let mut selectors = Vec::with_capacity(values.len());
    for value in values {
        let stream = value
            .as_dictionary()
            .ok_or_else(|| Error::InvalidRtsp("TEARDOWN stream is not a dictionary".to_owned()))?;
        let stream_type: u16 = optional_u64(stream, "type")?
            .ok_or_else(|| Error::InvalidRtsp("TEARDOWN stream has no type".to_owned()))?
            .try_into()
            .map_err(|_| Error::InvalidRtsp("TEARDOWN stream type exceeds u16".to_owned()))?;
        let connection_id = optional_u64_identifier(stream, "streamConnectionID")?;
        selectors.push(TeardownSelector {
            stream_type,
            connection_id,
        });
    }
    Ok(selectors)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use x25519_dalek::{PublicKey, StaticSecret};

    use super::*;
    use crate::{HeaderMap, SignerError, Tlv8, Tlv8Item, Tlv8Limits, TlvType};

    #[derive(Debug)]
    struct FixedSigner;

    impl MfiSigner for FixedSigner {
        fn protocol_major(&self) -> u8 {
            3
        }

        fn certificate(&mut self) -> std::result::Result<Vec<u8>, SignerError> {
            Ok(vec![0x30; 608])
        }

        fn sign_digest(&mut self, _digest: &[u8; 32]) -> std::result::Result<Vec<u8>, SignerError> {
            Ok(vec![0x44; 64])
        }
    }

    #[derive(Debug, Default)]
    struct FixedPorts;

    impl PortProvider for FixedPorts {
        fn session_ports(&mut self, _timing: &TimingDescriptor) -> Result<SessionPorts> {
            Ok(SessionPorts {
                timing_port: port(47_000),
                event_port: port(47_001),
                keep_alive_port: Some(port(47_002)),
            })
        }

        fn stream_ports(&mut self, stream: &StreamDescriptor) -> Result<StreamPorts> {
            Ok(match stream {
                StreamDescriptor::Screen(_) => StreamPorts::Screen {
                    data_port: port(48_000),
                },
                StreamDescriptor::Audio(_) => StreamPorts::Audio {
                    data_port: port(48_001),
                    control_port: port(48_002),
                },
                StreamDescriptor::DataChannel(_) => StreamPorts::DataChannel {
                    data_port: port(48_003),
                },
                StreamDescriptor::MessageChannel(_) => StreamPorts::MessageChannel,
            })
        }
    }

    fn port(port: u16) -> NonZeroU16 {
        NonZeroU16::new(port).expect("non-zero test port")
    }

    fn empty_info() -> InfoPayload {
        let mut info = Dictionary::new();
        info.insert("features".to_owned(), Value::Integer(7_u64.into()));
        InfoPayload::from_dictionary(info, PlistLimits::default()).expect("info")
    }

    fn new_receiver() -> (TempDir, AirPlayReceiver<FixedSigner>) {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let config = ReceiverConfig::new("3939", "cp-session-1", empty_info())
            .with_enabled_features(vec!["viewAreas".to_owned(), "altScreen".to_owned()]);
        let receiver =
            AirPlayReceiver::new(config, store, identity, FixedSigner).expect("receiver");
        (temporary, receiver)
    }

    fn request(
        method: &str,
        target: &str,
        cseq: u64,
        content_type: Option<&str>,
        session: bool,
        body: Vec<u8>,
    ) -> RtspRequest {
        let mut headers = HeaderMap::from([("cseq".to_owned(), cseq.to_string())]);
        if let Some(content_type) = content_type {
            headers.insert("content-type".to_owned(), content_type.to_owned());
        }
        if session {
            headers.insert("session".to_owned(), "cp-session-1".to_owned());
        }
        if !body.is_empty() {
            headers.insert("content-length".to_owned(), body.len().to_string());
        }
        RtspRequest {
            method: method.to_owned(),
            target: target.to_owned(),
            version: "RTSP/1.0".to_owned(),
            headers,
            body,
        }
    }

    fn encoded_dictionary(dictionary: Dictionary) -> Vec<u8> {
        plist_codec::encode(&Value::Dictionary(dictionary), PlistLimits::default()).expect("plist")
    }

    fn install_pair_verification(receiver: &mut AirPlayReceiver<FixedSigner>) {
        receiver.verification = Some(PairVerification::new("phone", [3; 32]).expect("pairing"));
        receiver.state = ReceiverState::AwaitingEncryptedControl;
    }

    fn authenticate(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        ports: &mut FixedPorts,
        cseq: u64,
    ) {
        install_pair_verification(receiver);
        let peer_secret = StaticSecret::from([11; 32]);
        let peer_public = PublicKey::from(&peer_secret);
        let mut body = vec![1];
        body.extend_from_slice(peer_public.as_bytes());
        let output = receiver
            .handle(
                &request(
                    "POST",
                    "rtsp://carplay.local/auth-setup",
                    cseq,
                    Some("application/octet-stream"),
                    false,
                    body,
                ),
                ControlTransport::Encrypted,
                ports,
            )
            .expect("auth setup");
        assert_eq!(output.response.status, 200);
        assert_eq!(receiver.state(), ReceiverState::Authenticated);
    }

    fn setup_session(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        ports: &mut FixedPorts,
        cseq: u64,
    ) -> ReceiverOutput {
        let golden = hex::decode("62706c6973743030d60102030405060708090a0b0c546e616d655864657669636549445a6d616341646472657373556d6f64656c5a74696d696e67506f72745f10116b656570416c6976654c6f77506f776572566950686f6e655541413a42425543433a44445a6950686f6e6531352c32111b620908151a232e343f535a606671740000000000000101000000000000000d00000000000000000000000000000075").expect("golden");
        receiver
            .handle(
                &request(
                    "SETUP",
                    "rtsp://fe80::a8ab:b5ff:feac:4cda/7436409126693455463",
                    cseq,
                    Some(PLIST_CONTENT_TYPE),
                    false,
                    golden,
                ),
                ControlTransport::Encrypted,
                ports,
            )
            .expect("session setup")
    }

    fn setup_streams(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        ports: &mut FixedPorts,
        cseq: u64,
    ) -> ReceiverOutput {
        let golden = hex::decode("62706c6973743030d101025773747265616d73a20308d20405060754747970655f101273747265616d436f6e6e656374696f6e4944106e1001d6090a0b0c0d0e0f101112131454747970655f101273747265616d436f6e6e656374696f6e494459617564696f547970655b617564696f466f726d61745864617461506f72745f100f6672616d65735065725061636b657410641003556d6564696112008000001117701101e0080b13161b20353739464b606a767f9193959ba0a300000000000001010000000000000015000000000000000000000000000000a6").expect("golden");
        receiver
            .handle(
                &request(
                    "SETUP",
                    "rtsp://fe80::a8ab:b5ff:feac:4cda/7436409126693455463",
                    cseq,
                    Some(PLIST_CONTENT_TYPE),
                    false,
                    golden,
                ),
                ControlTransport::Encrypted,
                ports,
            )
            .expect("stream setup")
    }

    #[test]
    fn parses_livi_stream_setup_golden_into_typed_descriptors() {
        let encoded = hex::decode("62706c6973743030d101025773747265616d73a20308d20405060754747970655f101273747265616d436f6e6e656374696f6e4944106e1001d6090a0b0c0d0e0f101112131454747970655f101273747265616d436f6e6e656374696f6e494459617564696f547970655b617564696f466f726d61745864617461506f72745f100f6672616d65735065725061636b657410641003556d6564696112008000001117701101e0080b13161b20353739464b606a767f9193959ba0a300000000000001010000000000000015000000000000000000000000000000a6").expect("hex");
        let dictionary =
            plist_codec::decode_dictionary(&encoded, PlistLimits::default()).expect("LIVI plist");
        let streams = parse_streams(&dictionary, 8).expect("streams");
        assert_eq!(streams.len(), 2);
        assert!(matches!(
            &streams[0],
            StreamDescriptor::Screen(ScreenDescriptor {
                kind: ScreenKind::Main,
                connection_id: 1
            })
        ));
        let StreamDescriptor::Audio(audio) = &streams[1] else {
            panic!("expected audio")
        };
        assert_eq!(audio.connection_id, 3);
        assert_eq!(audio.audio_format, 0x80_0000);
        assert_eq!(
            audio
                .microphone
                .as_ref()
                .map(|mic| mic.peer_data_port.get()),
            Some(6000)
        );
    }

    #[test]
    fn stream_descriptions_name_types_and_fields_without_values() {
        let mut stream = Dictionary::new();
        stream.insert("type".to_owned(), Value::Integer(120_u64.into()));
        stream.insert("shk".to_owned(), Value::Data(vec![0xaa; 32]));
        let mut dictionary = Dictionary::new();
        dictionary.insert(
            "streams".to_owned(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );

        assert_eq!(describe_streams(&dictionary), "type 120 [type,shk]");
    }

    #[test]
    fn parses_high_bit_stream_connection_id_from_apple_signed_bplist_integer() {
        const CONNECTION_ID: u64 = 16_768_230_515_396_397_400;

        let mut stream = Dictionary::new();
        stream.insert(
            "type".to_owned(),
            Value::Integer(u64::from(MAIN_SCREEN_STREAM_TYPE).into()),
        );
        stream.insert(
            "streamConnectionID".to_owned(),
            Value::Integer((CONNECTION_ID as i64).into()),
        );
        let mut dictionary = Dictionary::new();
        dictionary.insert(
            "streams".to_owned(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );

        assert_eq!(
            parse_streams(&dictionary, 8).expect("Apple high-bit connection ID"),
            vec![StreamDescriptor::Screen(ScreenDescriptor {
                kind: ScreenKind::Main,
                connection_id: CONNECTION_ID,
            })]
        );
    }

    #[test]
    fn strict_parameter_and_flush_parsers_reject_duplicates() {
        let limits = ReceiverLimits::default();
        assert!(parse_parameter_values(b"volume: -10\r\nvolume: -20\r\n", limits).is_err());
        assert!(parse_parameter_names(b"volume\r\nvolume\r\n", limits).is_err());
        assert!(parse_rtp_info("seq=1;seq=2").is_err());
        assert_eq!(
            parse_rtp_info("seq=7;rtptime=99").expect("RTP-Info"),
            FlushPoint {
                sequence: Some(7),
                rtp_time: Some(99)
            }
        );
    }

    #[test]
    fn routes_pair_setup_and_marks_a_replayed_cseq_terminal() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        let body = Tlv8::new(vec![
            Tlv8Item::new(TlvType::STATE, [1]),
            Tlv8Item::new(TlvType::METHOD, [0]),
        ])
        .encode(Tlv8Limits::default())
        .expect("M1");
        let mut pair_setup = request(
            "POST",
            "/pair-setup",
            1,
            Some(CARPLAY_PAIRING_REQUEST_CONTENT_TYPE),
            false,
            body,
        );
        pair_setup
            .headers
            .insert("x-apple-client-name".to_owned(), "Neel’s iPhone".to_owned());
        let first = receiver
            .handle(&pair_setup, ControlTransport::Cleartext, &mut ports)
            .expect("pair setup");
        assert_eq!(first.response.status, 200);
        assert_eq!(
            first
                .response
                .headers
                .get("content-type")
                .map(String::as_str),
            Some(PAIRING_CONTENT_TYPE)
        );
        let m2 = Tlv8::decode(&first.response.body, Tlv8Limits::default()).expect("M2");
        assert_eq!(m2.required_unique(TlvType::STATE).expect("state"), [2]);

        let replay = receiver
            .handle(
                &request("OPTIONS", "*", 1, None, false, Vec::new()),
                ControlTransport::Cleartext,
                &mut ports,
            )
            .expect("replay response");
        assert_eq!(replay.response.status, 400);
        assert_eq!(receiver.state(), ReceiverState::Failed);
        assert!(matches!(
            replay.events.as_slice(),
            [ReceiverEvent::Reset { .. }]
        ));
    }

    #[test]
    fn accepts_out_of_order_cseq_but_rejects_a_reused_one() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        for cseq in [5, 3] {
            let output = receiver
                .handle(
                    &request("OPTIONS", "*", cseq, None, false, Vec::new()),
                    ControlTransport::Cleartext,
                    &mut ports,
                )
                .expect("options");
            assert_eq!(output.response.status, 200);
        }
        let replay = receiver
            .handle(
                &request("OPTIONS", "*", 3, None, false, Vec::new()),
                ControlTransport::Cleartext,
                &mut ports,
            )
            .expect("replay response");
        assert_eq!(replay.response.status, 400);
        assert_eq!(receiver.state(), ReceiverState::Failed);
    }

    #[test]
    fn routes_carplay_octet_stream_pair_verify_request() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        let body = Tlv8::new(vec![
            Tlv8Item::new(TlvType::STATE, [1]),
            Tlv8Item::new(TlvType::PUBLIC_KEY, [7; 32]),
        ])
        .encode(Tlv8Limits::default())
        .expect("pair-verify M1");
        let output = receiver
            .handle(
                &request(
                    "POST",
                    "/pair-verify",
                    0,
                    Some(CARPLAY_VERIFY_REQUEST_CONTENT_TYPE),
                    false,
                    body,
                ),
                ControlTransport::Cleartext,
                &mut ports,
            )
            .expect("pair verify");
        assert_eq!(output.response.status, 200);
        assert_eq!(
            output
                .response
                .headers
                .get("content-type")
                .map(String::as_str),
            Some(PAIRING_CONTENT_TYPE)
        );
    }

    fn pairing_exchange(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        target: &str,
        cseq: u64,
        items: Vec<Tlv8Item>,
    ) -> ReceiverOutput {
        receiver
            .handle(
                &request(
                    "POST",
                    target,
                    cseq,
                    Some(PAIRING_CONTENT_TYPE),
                    false,
                    Tlv8::new(items).encode(Tlv8Limits::default()).expect("TLV"),
                ),
                ControlTransport::Cleartext,
                &mut FixedPorts,
            )
            .expect("pairing response")
    }

    fn verify_controller(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        cseq: u64,
        controller: &ed25519_dalek::SigningKey,
    ) -> ReceiverOutput {
        let secret = StaticSecret::from([13; 32]);
        let public = PublicKey::from(&secret).to_bytes();
        let output = pairing_exchange(
            receiver,
            "/pair-verify",
            cseq,
            vec![
                Tlv8Item::new(TlvType::STATE, [1]),
                Tlv8Item::new(TlvType::PUBLIC_KEY, public),
            ],
        );
        let m2 = Tlv8::decode(&output.response.body, Tlv8Limits::default()).expect("M2");
        let accessory: [u8; 32] = m2
            .required_unique(TlvType::PUBLIC_KEY)
            .expect("public key")
            .try_into()
            .expect("key length");
        let shared = secret.diffie_hellman(&PublicKey::from(accessory));
        let mut key = [0; 32];
        crate::crypto::hkdf_sha512(
            shared.as_bytes(),
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
            &mut key,
        )
        .expect("key");
        let signed = [public.as_slice(), b"phone-1", accessory.as_slice()].concat();
        let sub = Tlv8::new(vec![
            Tlv8Item::new(TlvType::IDENTIFIER, b"phone-1"),
            Tlv8Item::new(TlvType::SIGNATURE, crate::crypto::sign(controller, &signed)),
        ])
        .encode(Tlv8Limits::default())
        .expect("sub TLV");
        let encrypted = crate::crypto::seal(
            &key,
            &crate::crypto::nonce_label(b"PV-Msg03").expect("nonce"),
            &sub,
            &[],
        )
        .expect("encrypt");
        pairing_exchange(
            receiver,
            "/pair-verify",
            cseq + 1,
            vec![
                Tlv8Item::new(TlvType::STATE, [3]),
                Tlv8Item::new(TlvType::ENCRYPTED_DATA, encrypted),
            ],
        )
    }

    #[test]
    fn unknown_controller_can_pair_and_verify_on_the_same_connection() {
        let (temporary, mut receiver) = new_receiver();
        let controller = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
        let failed = verify_controller(&mut receiver, 1, &controller);
        assert_eq!(failed.response.status, 200);
        let failure = Tlv8::decode(&failed.response.body, Tlv8Limits::default()).expect("failure");
        assert_eq!(failure.required_unique(TlvType::ERROR).expect("error"), [2]);
        assert!(failed.control_keys.is_none());
        assert!(failed.events.is_empty());
        assert_eq!(receiver.state(), ReceiverState::Cleartext);
        assert!(receiver.controller_id().is_none());
        assert_eq!(
            receiver.pair_verify_failure_reason(),
            Some("pair-verify controller pairing is not stored")
        );

        let output = pairing_exchange(
            &mut receiver,
            "/pair-setup",
            3,
            vec![
                Tlv8Item::new(TlvType::STATE, [1]),
                Tlv8Item::new(TlvType::METHOD, [0]),
            ],
        );
        assert_eq!(output.response.status, 200);
        let m2 = Tlv8::decode(&output.response.body, Tlv8Limits::default()).expect("setup M2");
        let client = srp::ClientG3072::<sha2_11::Sha512>::new();
        let secret = [17; 32];
        let verifier = client
            .process_reply(
                &secret,
                b"Pair-Setup",
                b"3939",
                m2.required_unique(TlvType::SALT).expect("salt"),
                m2.required_unique(TlvType::PUBLIC_KEY).expect("public key"),
            )
            .expect("SRP client");
        let public = crate::srp_server::left_pad::<384>(&client.compute_public_ephemeral(&secret))
            .expect("padded public key");
        let session_key = srp::utils::compute_hash::<sha2_11::Sha512>(verifier.key());
        let proof = srp::utils::compute_m1_rfc5054::<sha2_11::Sha512>(
            &<srp::groups::G3072 as srp::Group>::generator(),
            true,
            b"Pair-Setup",
            m2.required_unique(TlvType::SALT).expect("salt"),
            &public,
            m2.required_unique(TlvType::PUBLIC_KEY).expect("public key"),
            &session_key,
        );
        let output = pairing_exchange(
            &mut receiver,
            "/pair-setup",
            4,
            vec![
                Tlv8Item::new(TlvType::STATE, [3]),
                Tlv8Item::new(TlvType::PUBLIC_KEY, public),
                Tlv8Item::new(TlvType::PROOF, proof.as_slice()),
            ],
        );
        let m4 = Tlv8::decode(&output.response.body, Tlv8Limits::default()).expect("setup M4");
        assert_eq!(
            m4.required_unique(TlvType::PROOF).expect("proof"),
            srp::utils::compute_m2::<sha2_11::Sha512>(&public, &proof, &session_key).as_slice()
        );
        let session_key = session_key.as_slice();
        let mut signing_key = [0; 32];
        crate::crypto::hkdf_sha512(
            session_key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
            &mut signing_key,
        )
        .expect("signing key");
        let public = controller.verifying_key().to_bytes();
        let signed = [signing_key.as_slice(), b"phone-1", public.as_slice()].concat();
        let sub = Tlv8::new(vec![
            Tlv8Item::new(TlvType::IDENTIFIER, b"phone-1"),
            Tlv8Item::new(TlvType::PUBLIC_KEY, public),
            Tlv8Item::new(
                TlvType::SIGNATURE,
                crate::crypto::sign(&controller, &signed),
            ),
        ])
        .encode(Tlv8Limits::default())
        .expect("setup sub TLV");
        let mut key = [0; 32];
        crate::crypto::hkdf_sha512(
            session_key,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
            &mut key,
        )
        .expect("encryption key");
        let encrypted = crate::crypto::seal(
            &key,
            &crate::crypto::nonce_label(b"PS-Msg05").expect("nonce"),
            &sub,
            &[],
        )
        .expect("encrypt");
        let output = pairing_exchange(
            &mut receiver,
            "/pair-setup",
            5,
            vec![
                Tlv8Item::new(TlvType::STATE, [5]),
                Tlv8Item::new(TlvType::ENCRYPTED_DATA, encrypted),
            ],
        );
        assert_eq!(receiver.pair_setup.state(), PairSetupState::Complete);
        assert_eq!(receiver.pair_verify.state(), PairVerifyState::AwaitingM1);
        assert_eq!(receiver.pair_verify_failure_reason(), None);
        assert!(output.control_keys.is_none());
        assert!(receiver.controller_id().is_none());
        assert_eq!(
            PairingStore::open(temporary.path())
                .expect("store")
                .pairing("phone-1")
                .expect("pairing"),
            Some(public)
        );

        let verified = verify_controller(&mut receiver, 6, &controller);
        assert!(verified.control_keys.is_some());
        assert_eq!(receiver.state(), ReceiverState::AwaitingEncryptedControl);
        assert_eq!(receiver.controller_id(), Some("phone-1"));
    }

    #[test]
    fn invalid_controller_signature_allows_pairing_but_not_encrypted_control() {
        let (temporary, mut receiver) = new_receiver();
        let stored = ed25519_dalek::SigningKey::from_bytes(&[12; 32]);
        PairingStore::open(temporary.path())
            .expect("store")
            .save_pairing("phone-1", stored.verifying_key().to_bytes())
            .expect("saved pairing");
        let controller = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
        let failed = verify_controller(&mut receiver, 1, &controller);
        assert_eq!(
            receiver.pair_verify_failure_reason(),
            Some("pair-verify controller signature is invalid")
        );
        assert!(failed.control_keys.is_none());
        assert!(receiver.controller_id().is_none());
        let output = pairing_exchange(
            &mut receiver,
            "/pair-setup",
            3,
            vec![
                Tlv8Item::new(TlvType::STATE, [1]),
                Tlv8Item::new(TlvType::METHOD, [0]),
            ],
        );
        assert_eq!(output.response.status, 200);
        assert_eq!(receiver.pair_setup.state(), PairSetupState::AwaitingM3);
        let denied = receiver
            .handle(
                &request("OPTIONS", "*", 4, None, false, Vec::new()),
                ControlTransport::Encrypted,
                &mut FixedPorts,
            )
            .expect("encrypted rejection");
        assert_eq!(denied.response.status, 455);
        assert_eq!(receiver.state(), ReceiverState::Failed);
        assert!(denied.control_keys.is_none());
    }

    #[test]
    fn failed_pair_verification_does_not_authorize_session_setup() {
        let (_temporary, mut receiver) = new_receiver();
        let controller = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
        verify_controller(&mut receiver, 1, &controller);
        let output = receiver
            .handle(
                &request(
                    "SETUP",
                    "/session",
                    3,
                    Some(PLIST_CONTENT_TYPE),
                    false,
                    encoded_dictionary(Dictionary::new()),
                ),
                ControlTransport::Cleartext,
                &mut FixedPorts,
            )
            .expect("rejected setup");
        assert_eq!(output.response.status, 455);
        assert_eq!(receiver.state(), ReceiverState::Failed);
        assert!(output.control_keys.is_none());
        assert_eq!(receiver.active_streams().len(), 0);
    }

    #[test]
    fn complete_livi_control_path_emits_typed_events_and_exact_setup_goldens() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        authenticate(&mut receiver, &mut ports, 1);

        let session = setup_session(&mut receiver, &mut ports, 2);
        assert_eq!(session.response.status, 200);
        assert!(!session.response.headers.contains_key("session"));
        assert_eq!(
            hex::encode(&session.response.body),
            "62706c6973743030d401020304050607085a74696d696e67506f7274596576656e74506f72745d6b656570416c697665506f72745f100f656e61626c6564466561747572657311b79811b79911b79aa2090a5976696577417265617359616c7453637265656e08111c263446494c4f525c0000000000000101000000000000000b00000000000000000000000000000066"
        );
        let [
            ReceiverEvent::SessionNegotiated {
                timing,
                ports: assigned,
            },
        ] = session.events.as_slice()
        else {
            panic!("session event")
        };
        assert_eq!(timing.peer.name.as_deref(), Some("iPhone"));
        assert_eq!(timing.peer_timing_port.map(NonZeroU16::get), Some(7010));
        assert_eq!(assigned.event_port.get(), 47_001);

        let info = receiver
            .handle(
                &request("GET", "/info", 3, None, false, Vec::new()),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("info");
        assert_eq!(info.response.body, receiver.info.as_bytes());
        assert!(!info.response.headers.contains_key("session"));
        assert_eq!(info.events, vec![ReceiverEvent::InfoRequested]);

        let record = receiver
            .handle(
                &request(
                    "RECORD",
                    "rtsp://carplay.local/7436409126693455463",
                    4,
                    None,
                    false,
                    Vec::new(),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("record");
        assert_eq!(record.events, vec![ReceiverEvent::RecordingStarted]);
        assert!(!record.response.headers.contains_key("session"));

        let streams = setup_streams(&mut receiver, &mut ports, 5);
        assert_eq!(
            hex::encode(&streams.response.body),
            "62706c6973743030d101025773747265616d73a20304d20506070854747970655864617461506f7274106e11bb80d40506090a0b0c0d0e5b636f6e74726f6c506f72745f101273747265616d436f6e6e656374696f6e4944106411bb8111bb821003080b13162e1b20292b3743585a5d600000000000000101000000000000000f00000000000000000000000000000062"
        );
        let livi_response = hex::decode("62706c6973743030d101025773747265616d73a20308d20405060754747970655864617461506f7274106e11bb80d4090a0b0c0d0e0f1054747970655864617461506f72745b636f6e74726f6c506f72745f101273747265616d436f6e6e656374696f6e4944106411bb8111bb821003080b13161b20292b2e373c455166686b6e0000000000000101000000000000001100000000000000000000000000000070").expect("LIVI response");
        assert_eq!(
            plist_codec::decode(&streams.response.body, PlistLimits::default())
                .expect("receiver response"),
            plist_codec::decode(&livi_response, PlistLimits::default()).expect("LIVI response")
        );
        assert_eq!(streams.events.len(), 2);
        let ReceiverEvent::StreamNegotiated {
            stream: StreamDescriptor::Audio(audio),
            ports: StreamPorts::Audio { control_port, .. },
        } = &streams.events[1]
        else {
            panic!("audio event")
        };
        assert_eq!(
            audio
                .microphone
                .as_ref()
                .map(|mic| mic.peer_data_port.get()),
            Some(6000)
        );
        assert_eq!(control_port.get(), 48_002);

        let set = receiver
            .handle(
                &request(
                    "SET_PARAMETER",
                    "/",
                    6,
                    Some(TEXT_PARAMETERS_CONTENT_TYPE),
                    true,
                    b"volume: -12.5\r\n".to_vec(),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("set parameter");
        assert!(matches!(set.events[0], ReceiverEvent::ParametersSet(_)));
        let get = receiver
            .handle(
                &request(
                    "GET_PARAMETER",
                    "/",
                    7,
                    Some(TEXT_PARAMETERS_CONTENT_TYPE),
                    true,
                    b"volume\r\n".to_vec(),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("get parameter");
        assert_eq!(get.response.body, b"volume: -12.5\r\n");

        let mut flush_request = request("FLUSH", "/", 8, None, true, Vec::new());
        flush_request
            .headers
            .insert("rtp-info".to_owned(), "seq=17;rtptime=99".to_owned());
        let flush = receiver
            .handle(&flush_request, ControlTransport::Encrypted, &mut ports)
            .expect("flush");
        assert_eq!(
            flush.events,
            vec![ReceiverEvent::Flushed(FlushPoint {
                sequence: Some(17),
                rtp_time: Some(99)
            })]
        );

        receiver
            .set_audio_feedback(vec![AudioFeedbackStream {
                stream_type: 102,
                sample_rate_hz: 48_000,
                anchor: Some(AudioFeedbackAnchor {
                    connection_id: 6_000,
                    synchronized_ntp: 0x1122_3344_5566_7788,
                    monotonic_nanoseconds: 8_765_432_100,
                    sample_time: 96_000,
                }),
            }])
            .expect("feedback snapshot");

        let feedback = receiver
            .handle(
                &request("POST", "/feedback", 9, None, true, Vec::new()),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("feedback");
        assert_eq!(feedback.response.status, 200);
        assert_eq!(feedback.events, vec![ReceiverEvent::KeepAlive]);
        assert_eq!(
            feedback
                .response
                .headers
                .get("content-type")
                .map(String::as_str),
            Some(PLIST_CONTENT_TYPE)
        );
        let feedback_body =
            plist_codec::decode_dictionary(&feedback.response.body, PlistLimits::default())
                .expect("feedback plist");
        let stream = feedback_body["streams"].as_array().expect("streams")[0]
            .as_dictionary()
            .expect("stream");
        assert_eq!(optional_u64(stream, "type").expect("type"), Some(102));
        assert_eq!(
            optional_u64(stream, "sampleRate").expect("sample rate"),
            Some(48_000)
        );
        assert_eq!(
            optional_u64(stream, "streamConnectionID").expect("connection ID"),
            Some(6_000)
        );
        assert_eq!(
            optional_u64(stream, "timestamp").expect("timestamp"),
            Some(0x1122_3344_5566_7788)
        );
        assert_eq!(
            optional_u64(stream, "timestampRawNs").expect("raw timestamp"),
            Some(8_765_432_100)
        );
        assert_eq!(
            optional_u64(stream, "sampleTime").expect("sample time"),
            Some(96_000)
        );

        let mut teardown_body = Dictionary::new();
        let mut audio = Dictionary::new();
        audio.insert("type".to_owned(), Value::Integer(100_u64.into()));
        teardown_body.insert(
            "streams".to_owned(),
            Value::Array(vec![Value::Dictionary(audio)]),
        );
        let partial = receiver
            .handle(
                &request(
                    "TEARDOWN",
                    "/",
                    10,
                    Some(PLIST_CONTENT_TYPE),
                    true,
                    encoded_dictionary(teardown_body),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("stream teardown");
        assert!(matches!(
            partial.events[0],
            ReceiverEvent::StreamsTornDown(_)
        ));
        assert_eq!(receiver.active_streams().len(), 1);

        let full = receiver
            .handle(
                &request("TEARDOWN", "/", 11, None, true, Vec::new()),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("session teardown");
        assert_eq!(full.events, vec![ReceiverEvent::SessionTornDown]);
        assert_eq!(receiver.state(), ReceiverState::Teardown);
    }

    #[test]
    fn cleartext_after_pair_verify_and_out_of_order_record_fail_closed() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        install_pair_verification(&mut receiver);
        let output = receiver
            .handle(
                &request("OPTIONS", "*", 1, None, false, Vec::new()),
                ControlTransport::Cleartext,
                &mut ports,
            )
            .expect("rejection");
        assert_eq!(output.response.status, 455);
        assert_eq!(receiver.state(), ReceiverState::Failed);

        let (_temporary, mut receiver) = new_receiver();
        authenticate(&mut receiver, &mut ports, 1);
        let output = receiver
            .handle(
                &request("RECORD", "/", 2, None, true, Vec::new()),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("rejection");
        assert_eq!(output.response.status, 455);
        assert_eq!(receiver.state(), ReceiverState::Failed);
    }

    #[test]
    fn present_mismatched_session_header_is_rejected() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        authenticate(&mut receiver, &mut ports, 1);
        setup_session(&mut receiver, &mut ports, 2);

        let mut info = request("GET", "/info", 3, None, false, Vec::new());
        info.headers
            .insert("session".into(), "wrong-session".into());
        let output = receiver
            .handle(&info, ControlTransport::Encrypted, &mut ports)
            .expect("session rejection");

        assert_eq!(output.response.status, 454);
        assert_eq!(receiver.state(), ReceiverState::Failed);
    }

    #[test]
    fn empty_dictionary_teardown_ends_the_session() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        authenticate(&mut receiver, &mut ports, 1);
        setup_session(&mut receiver, &mut ports, 2);
        let record = receiver
            .handle(
                &request("RECORD", "/", 3, None, false, Vec::new()),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("record");
        assert_eq!(record.response.status, 200);

        let teardown = receiver
            .handle(
                &request(
                    "TEARDOWN",
                    "rtsp://carplay.local/1",
                    4,
                    Some(PLIST_CONTENT_TYPE),
                    false,
                    encoded_dictionary(Dictionary::new()),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("teardown");

        assert_eq!(teardown.response.status, 200);
        assert_eq!(teardown.events, vec![ReceiverEvent::SessionTornDown]);
        assert_eq!(receiver.state(), ReceiverState::Teardown);
    }

    fn video_receiver() -> (TempDir, AirPlayReceiver<FixedSigner>) {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let mut entries = Dictionary::new();
        entries.insert("videoPlaybackInfo".to_owned(), Value::Boolean(true));
        let config = ReceiverConfig::new("3939", "cp-session-1", empty_info())
            .with_enabled_features(vec!["viewAreas".to_owned()])
            .with_conditional_feature("videoPlayback", entries);
        let receiver =
            AirPlayReceiver::new(config, store, identity, FixedSigner).expect("receiver");
        (temporary, receiver)
    }

    fn offer_session(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        ports: &mut FixedPorts,
        offered: &[&str],
    ) -> Dictionary {
        let mut body = Dictionary::new();
        body.insert("timingPort".to_owned(), Value::Integer(7_010_u64.into()));
        body.insert("keepAliveLowPower".to_owned(), Value::Boolean(true));
        body.insert(
            "features".to_owned(),
            Value::Array(
                offered
                    .iter()
                    .map(|feature| Value::String((*feature).to_owned()))
                    .collect(),
            ),
        );
        let output = receiver
            .handle(
                &request(
                    "SETUP",
                    "rtsp://fe80::1/1",
                    2,
                    Some(PLIST_CONTENT_TYPE),
                    false,
                    encoded_dictionary(body),
                ),
                ControlTransport::Encrypted,
                ports,
            )
            .expect("session setup");
        assert_eq!(output.response.status, 200);
        plist_codec::decode_dictionary(&output.response.body, PlistLimits::default())
            .expect("session response")
    }

    fn enabled(response: &Dictionary) -> Vec<&str> {
        response
            .get("enabledFeatures")
            .and_then(Value::as_array)
            .expect("enabled features")
            .iter()
            .filter_map(Value::as_string)
            .collect()
    }

    fn info_dictionary(
        receiver: &mut AirPlayReceiver<FixedSigner>,
        ports: &mut FixedPorts,
    ) -> Dictionary {
        let output = receiver
            .handle(
                &request("GET", "/info", 3, None, true, Vec::new()),
                ControlTransport::Encrypted,
                ports,
            )
            .expect("info");
        assert_eq!(output.response.status, 200);
        plist_codec::decode_dictionary(&output.response.body, PlistLimits::default())
            .expect("info plist")
    }

    #[test]
    fn conditional_feature_is_enabled_only_when_the_phone_offers_it() {
        let mut ports = FixedPorts;
        let (_temporary, mut declined) = video_receiver();
        authenticate(&mut declined, &mut ports, 1);
        let response = offer_session(&mut declined, &mut ports, &["hevc"]);
        assert_eq!(enabled(&response), vec!["viewAreas"]);
        assert!(declined.negotiated_features().is_empty());
        assert!(!info_dictionary(&mut declined, &mut ports).contains_key("videoPlaybackInfo"));

        let (_temporary, mut accepted) = video_receiver();
        authenticate(&mut accepted, &mut ports, 1);
        let response = offer_session(&mut accepted, &mut ports, &["videoPlayback", "hevc"]);
        assert_eq!(enabled(&response), vec!["viewAreas", "videoPlayback"]);
        assert_eq!(accepted.negotiated_features(), ["videoPlayback"]);
        assert_eq!(
            info_dictionary(&mut accepted, &mut ports).get("videoPlaybackInfo"),
            Some(&Value::Boolean(true))
        );
    }

    #[test]
    fn video_playback_stream_is_a_message_channel_without_a_port() {
        let mut ports = FixedPorts;
        let (_temporary, mut receiver) = video_receiver();
        authenticate(&mut receiver, &mut ports, 1);
        offer_session(&mut receiver, &mut ports, &["videoPlayback"]);
        let mut stream = Dictionary::new();
        stream.insert(
            "type".to_owned(),
            Value::Integer(u64::from(DATA_CHANNEL_STREAM_TYPE).into()),
        );
        stream.insert(
            "clientTypeUUID".to_owned(),
            Value::String(VIDEO_PLAYBACK_CLIENT_TYPE.to_lowercase()),
        );
        stream.insert(
            "clientUUID".to_owned(),
            Value::String("client-1".to_owned()),
        );
        let mut body = Dictionary::new();
        body.insert(
            "streams".to_owned(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let output = receiver
            .handle(
                &request(
                    "SETUP",
                    "/",
                    4,
                    Some(PLIST_CONTENT_TYPE),
                    true,
                    encoded_dictionary(body),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("message channel setup");
        assert_eq!(output.response.status, 200);
        assert_eq!(
            output.events,
            vec![ReceiverEvent::StreamNegotiated {
                stream: StreamDescriptor::MessageChannel(MessageChannelDescriptor {
                    stream_id: 1,
                    client_type_uuid: VIDEO_PLAYBACK_CLIENT_TYPE.to_owned(),
                    client_uuid: Some("client-1".to_owned()),
                }),
                ports: StreamPorts::MessageChannel,
            }]
        );
        let response =
            plist_codec::decode_dictionary(&output.response.body, PlistLimits::default())
                .expect("response plist");
        let stream = response
            .get("streams")
            .and_then(Value::as_array)
            .expect("streams")[0]
            .as_dictionary()
            .expect("stream");
        assert_eq!(optional_u64(stream, "streamID").expect("id"), Some(1));
        assert!(!stream.contains_key("dataPort"));

        let mut params = Dictionary::new();
        params.insert("data".to_owned(), Value::Data(b"bplist00payload".to_vec()));
        let mut command = Dictionary::new();
        command.insert("params".to_owned(), Value::Dictionary(params));
        let mut message = request(
            "POST",
            "/command",
            5,
            Some(PLIST_CONTENT_TYPE),
            false,
            encoded_dictionary(command.clone()),
        );
        message
            .headers
            .insert("x-apple-streamid".to_owned(), "1".to_owned());
        let output = receiver
            .handle(&message, ControlTransport::Encrypted, &mut ports)
            .expect("stream message");
        assert_eq!(output.response.status, 200);
        assert_eq!(
            output.events,
            vec![ReceiverEvent::StreamMessage {
                stream_id: 1,
                payload: b"bplist00payload".to_vec(),
            }]
        );

        let mut stranger = request(
            "POST",
            "/command",
            6,
            Some(PLIST_CONTENT_TYPE),
            false,
            encoded_dictionary(command),
        );
        stranger
            .headers
            .insert("x-apple-streamid".to_owned(), "9".to_owned());
        let output = receiver
            .handle(&stranger, ControlTransport::Encrypted, &mut ports)
            .expect("unknown stream message");
        assert_eq!(output.response.status, 400);
    }

    #[test]
    fn wireless_data_channel_setup_after_record_is_negotiated() {
        const SEED: i64 = -3_431_997_079_003_895_594;

        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        authenticate(&mut receiver, &mut ports, 1);
        setup_session(&mut receiver, &mut ports, 2);
        receiver
            .handle(
                &request("RECORD", "/", 3, None, false, Vec::new()),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("record");

        let mut stream = Dictionary::new();
        stream.insert("controlType".to_owned(), Value::Integer(2_u64.into()));
        stream.insert(
            "channelID".to_owned(),
            Value::String("DA6501B1-1452-4417-AE27-ED8E309DEBCE".to_owned()),
        );
        stream.insert("seed".to_owned(), Value::Integer(SEED.into()));
        stream.insert(
            "clientUUID".to_owned(),
            Value::String("11F965B7-8653-4A25-B82E-D9416C05FE68".to_owned()),
        );
        stream.insert(
            "type".to_owned(),
            Value::Integer(u64::from(DATA_CHANNEL_STREAM_TYPE).into()),
        );
        stream.insert("wantsDedicatedSocket".to_owned(), Value::Boolean(true));
        stream.insert("sendMessageAsIs".to_owned(), Value::Boolean(true));
        stream.insert(
            "clientTypeUUID".to_owned(),
            Value::String("1910A70F-DBC0-4242-AF95-115DB30604E1".to_owned()),
        );
        let mut body = Dictionary::new();
        body.insert(
            "streams".to_owned(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let output = receiver
            .handle(
                &request(
                    "SETUP",
                    "rtsp://fe80::7a20:51ff:fed2:bb5e/6604420504429808693",
                    4,
                    Some(PLIST_CONTENT_TYPE),
                    false,
                    encoded_dictionary(body),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("data channel setup");

        assert_eq!(output.response.status, 200);
        let descriptor = DataChannelDescriptor {
            seed: SEED as u64,
            channel_id: Some("DA6501B1-1452-4417-AE27-ED8E309DEBCE".to_owned()),
            client_type_uuid: Some("1910A70F-DBC0-4242-AF95-115DB30604E1".to_owned()),
        };
        assert_eq!(
            output.events,
            vec![ReceiverEvent::StreamNegotiated {
                stream: StreamDescriptor::DataChannel(descriptor.clone()),
                ports: StreamPorts::DataChannel {
                    data_port: port(48_003)
                },
            }]
        );
        let response =
            plist_codec::decode_dictionary(&output.response.body, PlistLimits::default())
                .expect("response plist");
        let streams = response
            .get("streams")
            .and_then(Value::as_array)
            .expect("streams");
        let stream = streams[0].as_dictionary().expect("stream");
        assert_eq!(optional_u64(stream, "type").expect("type"), Some(130));
        assert_eq!(optional_u64(stream, "streamID").expect("id"), Some(1));
        assert_eq!(
            optional_u64(stream, "dataPort").expect("port"),
            Some(48_003)
        );
        assert!(
            receiver
                .active_data_stream_key(
                    StreamDescriptor::DataChannel(descriptor).key(),
                    DataStreamDirection::Output
                )
                .is_ok()
        );
        assert_eq!(receiver.state(), ReceiverState::Recording);
    }

    #[test]
    fn duplicate_stream_and_malformed_teardown_reset_every_active_stream() {
        let (_temporary, mut receiver) = new_receiver();
        let mut ports = FixedPorts;
        authenticate(&mut receiver, &mut ports, 1);
        setup_session(&mut receiver, &mut ports, 2);
        setup_streams(&mut receiver, &mut ports, 3);
        let duplicate = setup_streams(&mut receiver, &mut ports, 4);
        assert_eq!(duplicate.response.status, 400);
        let [ReceiverEvent::Reset { streams, .. }] = duplicate.events.as_slice() else {
            panic!("reset event")
        };
        assert_eq!(streams.len(), 2);
        assert_eq!(receiver.state(), ReceiverState::Failed);

        let (_temporary, mut receiver) = new_receiver();
        authenticate(&mut receiver, &mut ports, 1);
        setup_session(&mut receiver, &mut ports, 2);
        setup_streams(&mut receiver, &mut ports, 3);
        let malformed = receiver
            .handle(
                &request(
                    "TEARDOWN",
                    "/",
                    4,
                    Some(PLIST_CONTENT_TYPE),
                    true,
                    b"not a plist".to_vec(),
                ),
                ControlTransport::Encrypted,
                &mut ports,
            )
            .expect("malformed teardown");
        assert_eq!(malformed.response.status, 400);
        assert_eq!(receiver.state(), ReceiverState::Failed);
    }
}
