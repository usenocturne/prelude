#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod gateway;
mod http;
mod play_queue;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod player;
mod playlist;
mod proxy;

use std::{
    io::Cursor,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
};

use plist::{Dictionary, Value};
use prelude_control::{ControlEvent, ControlEventDecoder};
use prelude_net::ReceiverEvent;

use crate::airplay_runtime::EventWorkerCommand;
pub(crate) use play_queue::PlayerStatus;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub(crate) use player::GstPlayerFactory;

use self::{
    gateway::{Gateway, GatewayNotice},
    play_queue::{PlayQueue, QueueAction},
    proxy::Proxy,
};

pub(crate) const VIDEO_PLAYBACK_FEATURE: &str = "videoPlayback";
const SCREEN_RESOURCE: u64 = 1;
const TRANSFER_BORROW: u64 = 3;
const TRANSFER_UNBORROW: u64 = 4;
const PRIORITY_USER_INITIATED: u64 = 500;
const BORROW_ID: &str = "VideoPlayback";
const NOTICE_CAPACITY: usize = 8;

pub(crate) trait PlayerFactory: Send {
    fn start(&self, url: &str, start_seconds: f64, muted: bool) -> Result<Box<dyn Player>, String>;
}

pub(crate) trait Player: Send {
    fn set_rate(&mut self, rate: f64);
    fn set_muted(&mut self, muted: bool);
    fn status(&self) -> PlayerStatus;
}

pub(crate) fn info_entries(features: u64) -> Dictionary {
    let mut info = Dictionary::new();
    info.insert("videoPlaybackAllowed".to_owned(), Value::Boolean(true));
    info.insert(
        "featuresEx".to_owned(),
        Value::String(features_ex(features)),
    );
    let mut capabilities = Dictionary::new();
    capabilities.insert("supportsFPSSecureStop".to_owned(), Value::Boolean(false));
    capabilities.insert(
        "supportsUIForAudioOnlyContent".to_owned(),
        Value::Boolean(false),
    );
    let mut entries = Dictionary::new();
    entries.insert("videoPlaybackInfo".to_owned(), Value::Dictionary(info));
    entries.insert(
        "playbackCapabilities".to_owned(),
        Value::Dictionary(capabilities),
    );
    entries
}

fn features_ex(features: u64) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = features.to_le_bytes();
    let length = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    let mut encoded = String::new();
    for chunk in bytes[..length].chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | u32::from(*byte) << (16 - 8 * index)
            });
        for index in 0..=chunk.len() {
            encoded.push(char::from(
                ALPHABET[(value >> (18 - 6 * index) & 0x3f) as usize],
            ));
        }
    }
    encoded
}

pub(crate) struct VideoSession {
    events: SyncSender<EventWorkerCommand>,
    factory: Option<Box<dyn PlayerFactory>>,
    scope_id: u32,
    decoder: ControlEventDecoder,
    enabled: bool,
    proxy: Option<Arc<Proxy>>,
    queue: PlayQueue,
    gateway: Option<Gateway>,
    player: Option<Box<dyn Player>>,
    current: Option<(String, bool)>,
    screen_borrowed: bool,
    notice_sender: SyncSender<GatewayNotice>,
    notices: Receiver<GatewayNotice>,
}

impl std::fmt::Debug for VideoSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoSession")
            .field("enabled", &self.enabled)
            .field("proxy", &self.proxy)
            .field("playing", &self.player.is_some())
            .field("screen_borrowed", &self.screen_borrowed)
            .finish_non_exhaustive()
    }
}

impl VideoSession {
    pub(crate) fn new(
        events: SyncSender<EventWorkerCommand>,
        factory: Option<Box<dyn PlayerFactory>>,
        scope_id: u32,
    ) -> Self {
        let (notice_sender, notices) = mpsc::sync_channel(NOTICE_CAPACITY);
        Self {
            events,
            factory,
            scope_id,
            decoder: ControlEventDecoder::default(),
            enabled: false,
            proxy: None,
            queue: PlayQueue::default(),
            gateway: None,
            player: None,
            current: None,
            screen_borrowed: false,
            notice_sender,
            notices,
        }
    }

    pub(crate) fn handle(&mut self, events: &[ReceiverEvent]) {
        for event in events {
            match event {
                ReceiverEvent::SessionNegotiated { timing, .. } => {
                    self.enabled = self.factory.is_some()
                        && timing
                            .offered_features
                            .iter()
                            .any(|feature| feature == VIDEO_PLAYBACK_FEATURE);
                    if self.enabled {
                        tracing::info!("the iPhone accepted CarPlay video playback");
                    }
                }
                ReceiverEvent::CommandReceived(body) if self.enabled => self.command(body),
                ReceiverEvent::StreamMessage { stream_id, payload } if self.enabled => {
                    self.stream_message(*stream_id, payload);
                }
                ReceiverEvent::SessionTornDown | ReceiverEvent::Reset { .. } => self.stop(false),
                _ => {}
            }
        }
        self.poll();
    }

    pub(crate) fn poll(&mut self) {
        while let Ok(notice) = self.notices.try_recv() {
            match notice {
                GatewayNotice::FairPlayProtected => {
                    tracing::warn!(
                        "this video is DRM protected; only unprotected AirPlay video can play on Prelude"
                    );
                }
            }
        }
    }

    fn command(&mut self, body: &[u8]) {
        if let Ok(Value::Dictionary(command)) = Value::from_reader(Cursor::new(body))
            && command.get("type").and_then(Value::as_string) != Some("setProxyParameters")
        {
            tracing::debug!(?command, "CarPlay command from the iPhone");
        }
        match self.decoder.decode(body) {
            Ok(Some(ControlEvent::VideoProxyOffered(offer))) => {
                self.proxy = Proxy::from_offer(&offer, self.scope_id).map(Arc::new);
                match &self.proxy {
                    Some(proxy) => {
                        tracing::info!(address = %proxy.address(), "the iPhone offered its video proxy");
                    }
                    None => tracing::warn!(?offer, "the iPhone offered an unusable video proxy"),
                }
            }
            Ok(Some(ControlEvent::VideoPlaybackRequested)) => {
                tracing::info!("the iPhone asked to show video");
                self.borrow_screen(true);
            }
            Ok(Some(ControlEvent::VideoMovedToAudioOnly)) => {
                tracing::info!("the iPhone moved the video to audio only");
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "ignoring an undecodable command for video"),
        }
    }

    fn stream_message(&mut self, stream_id: u64, payload: &[u8]) {
        let message = match Value::from_reader(Cursor::new(payload)) {
            Ok(Value::Dictionary(message)) => message,
            Ok(_) | Err(_) => {
                tracing::debug!(
                    stream_id,
                    bytes = payload.len(),
                    "ignoring a non-dictionary video stream message"
                );
                return;
            }
        };
        let polling = message.get("type").and_then(Value::as_string) == Some("playbackInfo");
        if !polling {
            tracing::info!(stream_id, message = %play_queue::describe(&message), "CarPlay video message");
        }
        let offset = self.gateway.as_ref().map_or(0.0, Gateway::offset_seconds);
        let status = self
            .player
            .as_ref()
            .map(|player| {
                let mut status = player.status();
                status.position = status.position.map(|position| position + offset);
                status
            })
            .unwrap_or_default();
        let handled = self.queue.handle(&message, &status);
        if let Some(reply) = handled.reply {
            self.reply(stream_id, &reply);
        }
        if let Some(action) = handled.action {
            self.apply(action);
        }
        if handled.started {
            self.send(allowed_command(self.player.is_some()));
            if self.player.is_some() {
                self.borrow_screen(true);
            }
        }
    }

    fn apply(&mut self, action: QueueAction) {
        match action {
            QueueAction::Load {
                url,
                start_seconds,
                muted,
            } => self.load(&url, start_seconds, muted),
            QueueAction::SetRate(rate) => {
                if let Some(player) = self.player.as_mut() {
                    player.set_rate(rate);
                }
            }
            QueueAction::Seek(seconds) => {
                let Some((url, muted)) = self.current.clone() else {
                    return;
                };
                tracing::info!(seconds, "CarPlay video restarts at a new position");
                self.load(&url, seconds, muted);
                let rate = self.queue.rate();
                if let Some(player) = self.player.as_mut() {
                    player.set_rate(rate);
                }
            }
            QueueAction::SetMuted(muted) => {
                if let Some((_, current)) = self.current.as_mut() {
                    *current = muted;
                }
                if let Some(player) = self.player.as_mut() {
                    player.set_muted(muted);
                }
            }
            QueueAction::Stop => self.stop(true),
        }
    }

    fn load(&mut self, url: &str, start_seconds: f64, muted: bool) {
        self.player = None;
        self.gateway = None;
        let Some(factory) = self.factory.as_ref() else {
            return;
        };
        let Some(proxy) = self.proxy.clone() else {
            tracing::warn!(url, "the iPhone sent a video before offering its proxy");
            return;
        };
        self.current = Some((url.to_owned(), muted));
        let gateway = match Gateway::open(proxy, self.notice_sender.clone(), start_seconds) {
            Ok(gateway) => gateway,
            Err(error) => {
                tracing::warn!(%error, "starting the CarPlay video gateway failed");
                return;
            }
        };
        let Some(local) = gateway.local_url(url) else {
            tracing::warn!(url, "the iPhone sent a video URL that is not HTTP");
            return;
        };
        match factory.start(&local, 0.0, muted) {
            Ok(player) => {
                tracing::info!(url, "CarPlay video loading");
                self.player = Some(player);
                self.gateway = Some(gateway);
            }
            Err(error) => tracing::warn!(%error, url, "starting the CarPlay video player failed"),
        }
    }

    fn stop(&mut self, tell_phone: bool) {
        let was_playing = self.player.take().is_some();
        self.gateway = None;
        self.current = None;
        self.queue = PlayQueue::default();
        if was_playing {
            tracing::info!("CarPlay video stopped");
        }
        if tell_phone && self.screen_borrowed {
            self.borrow_screen(false);
        }
        self.screen_borrowed &= tell_phone;
    }

    fn borrow_screen(&mut self, borrow: bool) {
        if self.screen_borrowed == borrow {
            return;
        }
        self.screen_borrowed = borrow;
        self.send(change_screen_command(borrow));
    }

    fn reply(&self, stream_id: u64, reply: &Value) {
        let mut encoded = Vec::new();
        if let Err(error) = reply.to_writer_binary(&mut encoded) {
            tracing::warn!(%error, "encoding a CarPlay video reply failed");
            return;
        }
        let mut params = Dictionary::new();
        params.insert("data".to_owned(), Value::Data(encoded));
        let mut body = Dictionary::new();
        body.insert("params".to_owned(), Value::Dictionary(params));
        self.queue_event(Value::Dictionary(body), Some(stream_id));
    }

    fn send(&self, body: Value) {
        self.queue_event(body, None);
    }

    fn queue_event(&self, body: Value, stream_id: Option<u64>) {
        match self
            .events
            .try_send(EventWorkerCommand::Command { body, stream_id })
        {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("CarPlay event channel is busy; dropped a video message");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn allowed_command(video: bool) -> Value {
    let mut params = Dictionary::new();
    params.insert("videoPlaybackAllowed".to_owned(), Value::Boolean(true));
    params.insert(
        "playbackStatus".to_owned(),
        Value::String(if video { "video" } else { "idle" }.to_owned()),
    );
    command("setVideoPlaybackAllowed", params)
}

fn change_screen_command(borrow: bool) -> Value {
    let mut screen = Dictionary::new();
    screen.insert("resourceID".to_owned(), Value::from(SCREEN_RESOURCE));
    screen.insert(
        "transferType".to_owned(),
        Value::from(if borrow {
            TRANSFER_BORROW
        } else {
            TRANSFER_UNBORROW
        }),
    );
    screen.insert("borrowID".to_owned(), Value::String(BORROW_ID.to_owned()));
    if borrow {
        screen.insert(
            "transferPriority".to_owned(),
            Value::from(PRIORITY_USER_INITIATED),
        );
        screen.insert(
            "unborrowConstraint".to_owned(),
            Value::from(PRIORITY_USER_INITIATED),
        );
    }
    let mut params = Dictionary::new();
    params.insert(
        "resources".to_owned(),
        Value::Array(vec![Value::Dictionary(screen)]),
    );
    command("changeModes", params)
}

fn command(name: &str, params: Dictionary) -> Value {
    let mut body = Dictionary::new();
    body.insert("type".to_owned(), Value::String(name.to_owned()));
    body.insert("params".to_owned(), Value::Dictionary(params));
    Value::Dictionary(body)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use prelude_net::{PeerIdentity, SessionPorts, TimingDescriptor};

    use super::*;
    use crate::carplay_video::proxy::tests::{fake_phone, response};

    #[derive(Default)]
    struct Recorded {
        started: Vec<(String, f64, bool)>,
        rates: Vec<f64>,
    }

    struct FakeFactory(Arc<Mutex<Recorded>>);

    struct FakePlayer(Arc<Mutex<Recorded>>);

    impl PlayerFactory for FakeFactory {
        fn start(
            &self,
            url: &str,
            start_seconds: f64,
            muted: bool,
        ) -> Result<Box<dyn Player>, String> {
            self.0
                .lock()
                .map_err(|_| "poisoned".to_owned())?
                .started
                .push((url.to_owned(), start_seconds, muted));
            Ok(Box::new(FakePlayer(Arc::clone(&self.0))))
        }
    }

    impl Player for FakePlayer {
        fn set_rate(&mut self, rate: f64) {
            if let Ok(mut recorded) = self.0.lock() {
                recorded.rates.push(rate);
            }
        }

        fn set_muted(&mut self, _muted: bool) {}

        fn status(&self) -> PlayerStatus {
            PlayerStatus {
                ready: true,
                playing: true,
                position: Some(5.0),
                ..PlayerStatus::default()
            }
        }
    }

    fn negotiated(offered: &[&str]) -> ReceiverEvent {
        let port = std::num::NonZeroU16::new(7_000).expect("port");
        ReceiverEvent::SessionNegotiated {
            timing: TimingDescriptor {
                peer_timing_port: None,
                low_power_keepalive: false,
                peer: PeerIdentity {
                    name: None,
                    device_id: None,
                    wifi_mac_address: None,
                    model: None,
                },
                offered_features: offered
                    .iter()
                    .map(|feature| (*feature).to_owned())
                    .collect(),
            },
            ports: SessionPorts {
                timing_port: port,
                event_port: port,
                keep_alive_port: None,
            },
        }
    }

    fn plist_bytes(value: Value) -> Vec<u8> {
        let mut encoded = Vec::new();
        value.to_writer_binary(&mut encoded).expect("plist");
        encoded
    }

    fn command_body(name: &str, params: Dictionary) -> Vec<u8> {
        plist_bytes(command(name, params))
    }

    fn stream_message(entries: &[(&str, Value)]) -> ReceiverEvent {
        let message: Dictionary = entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        ReceiverEvent::StreamMessage {
            stream_id: 2,
            payload: plist_bytes(Value::Dictionary(message)),
        }
    }

    fn drain(receiver: &Receiver<EventWorkerCommand>) -> Vec<(Option<String>, Option<u64>)> {
        let mut sent = Vec::new();
        while let Ok(EventWorkerCommand::Command { body, stream_id }) = receiver.try_recv() {
            let name = body
                .as_dictionary()
                .and_then(|body| body.get("type"))
                .and_then(Value::as_string)
                .map(str::to_owned);
            sent.push((name, stream_id));
        }
        sent
    }

    #[test]
    fn features_ex_is_little_endian_base64_without_padding() {
        assert_eq!(features_ex(0x0061_5653_aee2), "4q5TVmE");
        assert_eq!(features_ex(0x80), "gA");
        assert_eq!(features_ex(0), "");
    }

    #[test]
    fn video_stays_off_unless_the_phone_offers_it() {
        let (events, received) = mpsc::sync_channel(16);
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut session = VideoSession::new(
            events,
            Some(Box::new(FakeFactory(Arc::clone(&recorded)))),
            0,
        );
        session.handle(&[
            negotiated(&["hevc"]),
            stream_message(&[
                ("type", Value::String("playbackInfo".to_owned())),
                ("kind", Value::String("request".to_owned())),
            ]),
        ]);
        assert!(drain(&received).is_empty());
    }

    #[test]
    fn play_queue_drives_the_player_and_the_screen() {
        let phone = fake_phone(|_, _| response("application/vnd.apple.mpegurl", "#EXTM3U\n"));
        let (events, received) = mpsc::sync_channel(16);
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let mut session = VideoSession::new(
            events,
            Some(Box::new(FakeFactory(Arc::clone(&recorded)))),
            0,
        );
        let mut proxy = Dictionary::new();
        proxy.insert(
            "proxyUrl".to_owned(),
            Value::String(phone.offer.url.clone()),
        );
        proxy.insert(
            "proxyPort".to_owned(),
            Value::from(u64::from(phone.offer.port)),
        );
        proxy.insert("proxyPsk".to_owned(), Value::Data(phone.offer.psk.clone()));
        proxy.insert(
            "proxyPskIdentity".to_owned(),
            Value::Data(phone.offer.identity.clone()),
        );
        proxy.insert(
            "proxyAuthorization".to_owned(),
            Value::String(phone.offer.authorization.clone()),
        );
        let mut item = Dictionary::new();
        item.insert(
            "Content-Location".to_owned(),
            Value::String("http://127.0.0.1:58539/master.m3u8".to_owned()),
        );
        item.insert("Start-Position".to_owned(), Value::Real(3.0));
        session.handle(&[
            negotiated(&["videoPlayback"]),
            ReceiverEvent::CommandReceived(command_body("setProxyParameters", proxy)),
            stream_message(&[
                ("type", Value::String("insertPlayQueueItem".to_owned())),
                ("kind", Value::String("request".to_owned())),
                ("messageID", Value::from(1_u64)),
                ("item", Value::Dictionary(item)),
            ]),
            stream_message(&[
                ("type", Value::String("setRate".to_owned())),
                ("rate", Value::Real(1.0)),
            ]),
        ]);
        {
            let recorded = recorded.lock().expect("recorded");
            assert_eq!(recorded.started.len(), 1);
            let (url, start, muted) = &recorded.started[0];
            assert!(url.starts_with("http://127.0.0.1:"));
            assert!(url.ends_with("/~/http/127.0.0.1:58539/master.m3u8"));
            assert_eq!((*start, *muted), (0.0, false));
            assert_eq!(recorded.rates, vec![1.0]);
        }
        assert_eq!(
            drain(&received),
            vec![
                (None, Some(2)),
                (Some("setVideoPlaybackAllowed".to_owned()), None),
                (Some("changeModes".to_owned()), None),
            ]
        );
        session.handle(&[stream_message(&[(
            "type",
            Value::String("stop".to_owned()),
        )])]);
        assert_eq!(
            drain(&received),
            vec![(Some("changeModes".to_owned()), None)]
        );
    }
}
