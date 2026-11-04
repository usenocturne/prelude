#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-media")),
    allow(dead_code)
)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use prelude_bluetooth_sco::{AirMode, IncomingSco, ScoLink, msbc};
use prelude_wireless::{
    bluez::{PairingPasskeys, PhoneKind},
    hfp::CallCodec,
};
use sonora::{
    AudioProcessing, AudioProcessingStats, Config, StreamConfig,
    config::{EchoCanceller, HighPassFilter, NoiseSuppression},
};

use crate::wireless_runtime::{SharedBluetoothAddress, SharedBluetoothControl, remember_phone};

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub(crate) use bridge::CallBridge;

const CAR_ADDRESS_POLL: Duration = Duration::from_millis(100);
const CVSD_RATE: u32 = 8_000;
const ECHO_REFERENCE_DELAY: Duration = Duration::from_millis(100);
const NARROWBAND_MARKER: &str = "narrowband-calls";

#[derive(Debug, Default)]
struct CallListeners {
    next_id: u64,
    current: Option<(u64, SyncSender<CallAudio>)>,
}

#[derive(Debug)]
pub(crate) struct CallAudio {
    link: ScoLink,
    codec: CallCodec,
    codecs: CallCodecs,
}

impl CallAudio {
    pub(crate) const fn codec(&self) -> CallCodec {
        self.codec
    }

    pub(crate) const fn mtu(&self) -> usize {
        self.link.mtu()
    }

    pub(crate) const fn sample_rate(&self) -> u32 {
        voice_rate(self.codec)
    }
}

pub(crate) const fn voice_rate(codec: CallCodec) -> u32 {
    match codec {
        CallCodec::Cvsd => CVSD_RATE,
        CallCodec::Msbc => msbc::SAMPLE_RATE,
    }
}

const fn air_mode(codec: CallCodec) -> AirMode {
    match codec {
        CallCodec::Cvsd => AirMode::Cvsd,
        CallCodec::Msbc => AirMode::Transparent,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CallCodecs {
    wideband: Arc<AtomicBool>,
    selected: Arc<Mutex<HashMap<String, CallCodec>>>,
    narrowband_requests: Arc<Mutex<HashSet<String>>>,
    marker: PathBuf,
}

impl CallCodecs {
    fn new(state_dir: &Path) -> Self {
        let marker = state_dir.join(NARROWBAND_MARKER);
        Self {
            wideband: Arc::new(AtomicBool::new(!marker.exists())),
            selected: Arc::default(),
            narrowband_requests: Arc::default(),
            marker,
        }
    }

    pub(crate) fn wideband(&self) -> bool {
        self.wideband.load(Ordering::Acquire)
    }

    pub(crate) fn select(&self, peer: &str, codec: CallCodec) {
        if let Ok(mut selected) = self.selected.lock() {
            selected.insert(peer_key(peer), codec);
        }
    }

    pub(crate) fn forget(&self, peer: &str) {
        let peer = peer_key(peer);
        if let Ok(mut selected) = self.selected.lock() {
            selected.remove(&peer);
        }
        if let Ok(mut requests) = self.narrowband_requests.lock() {
            requests.remove(&peer);
        }
    }

    pub(crate) fn take_narrowband_request(&self, peer: &str) -> bool {
        self.narrowband_requests
            .lock()
            .is_ok_and(|mut requests| requests.remove(&peer_key(peer)))
    }

    fn selected(&self, peer: &str) -> CallCodec {
        self.selected
            .lock()
            .ok()
            .and_then(|selected| selected.get(&peer_key(peer)).copied())
            .unwrap_or(CallCodec::Cvsd)
    }

    fn fall_back_to_narrowband(&self, peer: &str) {
        self.wideband.store(false, Ordering::Release);
        let peer = peer_key(peer);
        if let Ok(mut selected) = self.selected.lock() {
            selected.insert(peer.clone(), CallCodec::Cvsd);
        }
        if let Ok(mut requests) = self.narrowband_requests.lock() {
            requests.insert(peer);
        }
        let remembered = self
            .marker
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&self.marker, b""));
        if let Err(error) = remembered {
            tracing::warn!(
                %error,
                path = %self.marker.display(),
                "remembering that HD Bluetooth calls do not work failed"
            );
        }
    }
}

fn peer_key(peer: &str) -> String {
    parse_address(peer).map_or_else(|| peer.trim().to_ascii_uppercase(), format_address)
}

#[derive(Debug, Clone)]
pub(crate) struct HandsFree {
    address: SharedBluetoothAddress,
    control: SharedBluetoothControl,
    passkeys: PairingPasskeys,
    state_dir: PathBuf,
    listeners: Arc<Mutex<CallListeners>>,
    usb_sessions: Arc<AtomicUsize>,
    expected: Arc<AtomicBool>,
    codecs: CallCodecs,
}

#[derive(Debug)]
pub(crate) struct UsbSession {
    sessions: Arc<AtomicUsize>,
}

impl Drop for UsbSession {
    fn drop(&mut self) {
        self.sessions.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HandsFree {
    pub(crate) fn new(
        address: SharedBluetoothAddress,
        control: SharedBluetoothControl,
        passkeys: PairingPasskeys,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            address,
            control,
            passkeys,
            codecs: CallCodecs::new(&state_dir),
            state_dir,
            listeners: Arc::default(),
            usb_sessions: Arc::default(),
            expected: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn call_codecs(&self) -> &CallCodecs {
        &self.codecs
    }

    pub(crate) fn set_bluetooth_expected(&self, expected: bool) {
        self.expected.store(expected, Ordering::Release);
    }

    pub(crate) fn wait_for_car_address(
        &self,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(address) = self.car_address() {
                return Some(address);
            }
            if !self.expected.load(Ordering::Acquire) || cancelled() || Instant::now() >= deadline {
                return None;
            }
            thread::sleep(CAR_ADDRESS_POLL);
        }
    }

    pub(crate) fn usb_session(&self) -> UsbSession {
        self.usb_sessions.fetch_add(1, Ordering::AcqRel);
        UsbSession {
            sessions: Arc::clone(&self.usb_sessions),
        }
    }

    pub(crate) fn projecting_over_usb(&self) -> bool {
        self.usb_sessions.load(Ordering::Acquire) > 0
    }

    pub(crate) fn take_passkey(&self, phone_address: &str) -> Option<u32> {
        self.passkeys
            .take(&format_address(parse_address(phone_address)?))
    }

    pub(crate) fn car_address(&self) -> Option<String> {
        if self.control.read().ok()?.is_none() {
            return None;
        }
        let address = (*self.address.read().ok()?)?;
        Some(format_address(address))
    }

    pub(crate) fn is_paired(&self, phone_address: &str) -> Option<bool> {
        let address = parse_address(phone_address)?;
        let control = self.control.read().ok()?.clone()?;
        control.is_paired(&format_address(address))
    }

    pub(crate) fn remember_phone(&self, phone_address: &str) {
        let Some(address) = parse_address(phone_address) else {
            return;
        };
        let address = format_address(address);
        if let Err(error) = remember_phone(
            &self.state_dir,
            &self.control,
            PhoneKind::AndroidAuto,
            &address,
        ) {
            tracing::warn!(phone = address, %error, "remembering the hands-free phone failed");
        }
    }

    pub(crate) fn connect_phone(&self, phone_address: &str) -> bool {
        let Some(address) = parse_address(phone_address) else {
            return false;
        };
        let Some(control) = self.control.read().ok().and_then(|control| control.clone()) else {
            return false;
        };
        control.connect(&format_address(address), PhoneKind::AndroidAuto);
        true
    }

    pub(crate) fn offer_call(&self, incoming: IncomingSco) {
        let peer = format_address(incoming.peer());
        let sender = self
            .listeners
            .lock()
            .ok()
            .and_then(|listeners| listeners.current.as_ref().map(|(_, sender)| sender.clone()));
        let Some(sender) = sender else {
            tracing::info!(
                peer,
                reason = "no Android Auto session is playing audio on the DAC",
                "refusing Bluetooth call audio"
            );
            return;
        };
        let codec = self.codecs.selected(&peer);
        let link = match incoming.accept(air_mode(codec)) {
            Ok(link) => link,
            Err(error) => {
                tracing::warn!(peer, ?codec, %error, "accepting Bluetooth call audio failed");
                return;
            }
        };
        let audio = CallAudio {
            link,
            codec,
            codecs: self.codecs.clone(),
        };
        let refused = match sender.try_send(audio) {
            Ok(()) => None,
            Err(TrySendError::Full(_)) => Some("the previous call audio is still starting"),
            Err(TrySendError::Disconnected(_)) => Some("the Android Auto session ended"),
        };
        match refused {
            None => tracing::info!(peer, ?codec, "the phone opened Bluetooth call audio"),
            Some(reason) => tracing::info!(peer, reason, "refusing Bluetooth call audio"),
        }
    }

    pub(crate) fn subscribe(&self) -> CallSubscription {
        let (sender, links) = mpsc::sync_channel(1);
        let id = match self.listeners.lock() {
            Ok(mut listeners) => {
                listeners.next_id = listeners.next_id.wrapping_add(1);
                let id = listeners.next_id;
                listeners.current = Some((id, sender));
                id
            }
            Err(_) => 0,
        };
        CallSubscription {
            listeners: Arc::clone(&self.listeners),
            id,
            links,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CallSubscription {
    listeners: Arc<Mutex<CallListeners>>,
    id: u64,
    links: Receiver<CallAudio>,
}

impl CallSubscription {
    pub(crate) fn try_next(&self) -> Option<CallAudio> {
        self.links.try_recv().ok()
    }
}

impl Drop for CallSubscription {
    fn drop(&mut self) {
        if let Ok(mut listeners) = self.listeners.lock()
            && listeners
                .current
                .as_ref()
                .is_some_and(|(id, _)| *id == self.id)
        {
            listeners.current = None;
        }
    }
}

fn parse_address(text: &str) -> Option<[u8; 6]> {
    let mut address = [0_u8; 6];
    let mut octets = text.trim().split(':');
    for byte in &mut address {
        let octet = octets.next()?;
        if octet.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(octet, 16).ok()?;
    }
    octets.next().is_none().then_some(address)
}

fn format_address(address: [u8; 6]) -> String {
    address
        .iter()
        .map(|octet| format!("{octet:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[derive(Debug, Default)]
pub(crate) struct Upsampler {
    previous: i16,
}

impl Upsampler {
    pub(crate) fn double_rate(&mut self, pcm_s16le: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(pcm_s16le.len() * 2);
        for sample in pcm_s16le.chunks_exact(2) {
            let current = i16::from_le_bytes([sample[0], sample[1]]);
            let midpoint = ((i32::from(self.previous) + i32::from(current)) / 2) as i16;
            output.extend_from_slice(&midpoint.to_le_bytes());
            output.extend_from_slice(&current.to_le_bytes());
            self.previous = current;
        }
        output
    }
}

#[derive(Debug)]
pub(crate) struct CallVoiceProcessor {
    processing: AudioProcessing,
    frame_bytes: usize,
    scheduled: VecDeque<(Instant, Vec<u8>)>,
    far_end: Vec<u8>,
    near_end: Vec<u8>,
    input: Vec<i16>,
    output: Vec<i16>,
}

impl CallVoiceProcessor {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let stream = StreamConfig::new(sample_rate, 1);
        let processing = AudioProcessing::builder()
            .config(Config {
                high_pass_filter: Some(HighPassFilter::default()),
                echo_canceller: Some(EchoCanceller::default()),
                noise_suppression: Some(NoiseSuppression::default()),
                gain_controller2: Some(prelude_media::voice_gain_controller()),
                ..Config::default()
            })
            .capture_config(stream)
            .render_config(stream)
            .build();
        let frame_samples = sample_rate as usize / 100;
        let frame_bytes = frame_samples * 2;
        Self {
            processing,
            frame_bytes,
            scheduled: VecDeque::new(),
            far_end: Vec::with_capacity(frame_bytes * 4),
            near_end: Vec::with_capacity(frame_bytes * 4),
            input: vec![0; frame_samples],
            output: vec![0; frame_samples],
        }
    }

    pub(crate) fn far_end(&mut self, pcm_s16le: &[u8], now: Instant) {
        self.scheduled
            .push_back((now + ECHO_REFERENCE_DELAY, pcm_s16le.to_vec()));
        self.release_far_end(now);
    }

    fn release_far_end(&mut self, now: Instant) {
        while let Some((due, _)) = self.scheduled.front()
            && *due <= now
        {
            if let Some((_, pcm_s16le)) = self.scheduled.pop_front() {
                self.far_end.extend_from_slice(&pcm_s16le);
            }
        }
        let complete = self.far_end.len() - self.far_end.len() % self.frame_bytes;
        for frame in self.far_end[..complete].chunks_exact(self.frame_bytes) {
            decode_samples(frame, &mut self.input);
            let _ = self
                .processing
                .process_render_i16(&self.input, &mut self.output);
        }
        self.far_end.drain(..complete);
    }

    pub(crate) fn near_end(&mut self, pcm_s16le: &[u8], now: Instant, uplink: &mut VecDeque<u8>) {
        self.release_far_end(now);
        self.near_end.extend_from_slice(pcm_s16le);
        let complete = self.near_end.len() - self.near_end.len() % self.frame_bytes;
        for frame in self.near_end[..complete].chunks_exact(self.frame_bytes) {
            decode_samples(frame, &mut self.input);
            if self
                .processing
                .process_capture_i16(&self.input, &mut self.output)
                .is_ok()
            {
                uplink.extend(self.output.iter().flat_map(|sample| sample.to_le_bytes()));
            } else {
                uplink.extend(frame);
            }
        }
        self.near_end.drain(..complete);
    }

    pub(crate) fn statistics(&self) -> &AudioProcessingStats {
        self.processing.statistics()
    }
}

fn decode_samples(pcm_s16le: &[u8], samples: &mut [i16]) {
    for (sample, bytes) in samples.iter_mut().zip(pcm_s16le.chunks_exact(2)) {
        *sample = i16::from_le_bytes([bytes[0], bytes[1]]);
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
mod bridge {
    use std::{
        collections::VecDeque,
        io,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use prelude_bluetooth_sco::{ScoLink, msbc};
    use prelude_media::{
        AudioFormatId, AudioFrameSender, AudioStreamId, LinuxMicrophoneBackend,
        MicrophoneBackendEvent,
    };
    use prelude_wireless::hfp::CallCodec;

    use super::{
        CallAudio, CallVoiceProcessor, Upsampler, decode_samples, format_address, voice_rate,
    };
    use crate::android_auto_runtime::AudioClock;

    const CALL_FORMAT: AudioFormatId = AudioFormatId::new(0x10);
    const CALL_OUTPUT_RATE: u32 = 16_000;
    const DOWNLINK_FRAMES_PER_SECOND: usize = 50;
    const UPLINK_LIMIT_FRAMES_PER_SECOND: usize = 10;
    const READ_BUFFER_BYTES: usize = 1_024;
    const LINK_TIMEOUT: Duration = Duration::from_millis(200);
    const WIDEBAND_PROBE: Duration = Duration::from_secs(2);

    #[derive(Debug)]
    pub(crate) struct CallBridge {
        peer: String,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<AudioClock>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PumpEnd {
        Closed,
        WidebandUndecodable,
    }

    impl CallBridge {
        pub(crate) fn start(
            audio: CallAudio,
            frames: AudioFrameSender,
            clock: AudioClock,
            microphone: Option<LinuxMicrophoneBackend>,
        ) -> io::Result<Self> {
            let CallAudio {
                link,
                codec,
                codecs,
            } = audio;
            link.set_timeouts(LINK_TIMEOUT)?;
            let peer = format_address(link.peer());
            let stop = Arc::new(AtomicBool::new(false));
            let thread = thread::Builder::new()
                .name("prelude-call-audio".to_owned())
                .spawn({
                    let stop = Arc::clone(&stop);
                    let peer = peer.clone();
                    move || {
                        let mut microphone = microphone;
                        let mut voice = CallVoiceProcessor::new(voice_rate(codec));
                        let mut voice_link = VoiceLink::new(codec);
                        let (clock, end) = pump(
                            &link,
                            &mut voice_link,
                            &frames,
                            clock,
                            &stop,
                            &mut microphone,
                            &mut voice,
                            &peer,
                        );
                        drop(link);
                        if end == PumpEnd::WidebandUndecodable {
                            tracing::warn!(
                                peer,
                                probe_ms = WIDEBAND_PROBE.as_millis() as u64,
                                "no HD call audio could be decoded; switching Bluetooth calls to narrowband"
                            );
                            codecs.fall_back_to_narrowband(&peer);
                        }
                        let statistics = voice.statistics();
                        let (frames_decoded, frames_concealed) = voice_link.frame_counts().unzip();
                        tracing::info!(
                            peer,
                            ?codec,
                            frames_decoded,
                            frames_concealed,
                            echo_delay_ms = ?statistics.delay_ms,
                            echo_return_loss_enhancement_db =
                                ?statistics.echo_return_loss_enhancement.map(f64::round),
                            "call echo canceller summary"
                        );
                        if let Some(microphone) = microphone
                            && let Err(error) = microphone.shutdown()
                        {
                            tracing::warn!(peer, %error, "stopping the call microphone failed");
                        }
                        clock
                    }
                })?;
            Ok(Self {
                peer,
                stop,
                thread: Some(thread),
            })
        }

        pub(crate) fn peer(&self) -> &str {
            &self.peer
        }

        pub(crate) fn is_finished(&self) -> bool {
            self.thread.as_ref().is_none_or(JoinHandle::is_finished)
        }

        pub(crate) fn finish(mut self) -> Option<AudioClock> {
            self.stop.store(true, Ordering::Release);
            self.thread.take()?.join().ok()
        }
    }

    impl Drop for CallBridge {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[derive(Debug)]
    enum VoiceLink {
        Cvsd(Upsampler),
        Msbc(Box<MsbcLink>),
    }

    #[derive(Debug, Default)]
    struct MsbcLink {
        receiver: msbc::Receiver,
        encoder: msbc::Encoder,
        decoded: Vec<i16>,
        outgoing: VecDeque<u8>,
    }

    impl MsbcLink {
        fn receive(&mut self, packet: &[u8], downlink: &mut Vec<u8>) {
            self.decoded.clear();
            self.receiver.push(packet, &mut self.decoded);
            downlink.extend(self.decoded.iter().flat_map(|sample| sample.to_le_bytes()));
        }

        fn transmit(&mut self, uplink: &mut VecDeque<u8>, length: usize) -> Vec<u8> {
            while self.outgoing.len() < length {
                let mut pcm = [0_i16; msbc::FRAME_SAMPLES];
                if uplink.len() >= msbc::FRAME_SAMPLES * 2 {
                    let captured: Vec<u8> = uplink.drain(..msbc::FRAME_SAMPLES * 2).collect();
                    decode_samples(&captured, &mut pcm);
                }
                self.outgoing.extend(self.encoder.encode(&pcm));
            }
            self.outgoing.drain(..length).collect()
        }
    }

    impl VoiceLink {
        fn new(codec: CallCodec) -> Self {
            match codec {
                CallCodec::Cvsd => Self::Cvsd(Upsampler::default()),
                CallCodec::Msbc => Self::Msbc(Box::default()),
            }
        }

        const fn codec(&self) -> CallCodec {
            match self {
                Self::Cvsd(_) => CallCodec::Cvsd,
                Self::Msbc(_) => CallCodec::Msbc,
            }
        }

        fn receive(&mut self, packet: &[u8], downlink: &mut Vec<u8>) {
            match self {
                Self::Cvsd(_) => downlink.extend_from_slice(packet),
                Self::Msbc(link) => link.receive(packet, downlink),
            }
        }

        fn mixer_pcm(&mut self, pcm_s16le: &[u8]) -> Vec<u8> {
            match self {
                Self::Cvsd(upsampler) => upsampler.double_rate(pcm_s16le),
                Self::Msbc(_) => pcm_s16le.to_vec(),
            }
        }

        fn transmit(&mut self, uplink: &mut VecDeque<u8>, length: usize) -> Vec<u8> {
            match self {
                Self::Cvsd(_) => take_uplink(uplink, length),
                Self::Msbc(link) => link.transmit(uplink, length),
            }
        }

        fn undecodable(&self) -> bool {
            matches!(self, Self::Msbc(link) if link.receiver.frames_decoded() == 0)
        }

        fn frame_counts(&self) -> Option<(u64, u64)> {
            match self {
                Self::Cvsd(_) => None,
                Self::Msbc(link) => Some((
                    link.receiver.frames_decoded(),
                    link.receiver.frames_concealed(),
                )),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pump(
        link: &ScoLink,
        voice_link: &mut VoiceLink,
        frames: &AudioFrameSender,
        mut clock: AudioClock,
        stop: &AtomicBool,
        microphone: &mut Option<LinuxMicrophoneBackend>,
        voice: &mut CallVoiceProcessor,
        peer: &str,
    ) -> (AudioClock, PumpEnd) {
        let bytes_per_second = voice_rate(voice_link.codec()) as usize * 2;
        let frame_bytes = bytes_per_second / DOWNLINK_FRAMES_PER_SECOND;
        let uplink_limit = bytes_per_second / UPLINK_LIMIT_FRAMES_PER_SECOND;
        let silence = vec![0_u8; frame_bytes];
        let started = Instant::now();
        let mut uplink = VecDeque::with_capacity(uplink_limit * 2);
        let mut packet = vec![0_u8; READ_BUFFER_BYTES];
        let mut downlink = Vec::with_capacity(frame_bytes * 2);
        let mut dropped_frames = 0_u64;
        while !stop.load(Ordering::Acquire) {
            if voice_link.undecodable() && started.elapsed() >= WIDEBAND_PROBE {
                return (clock, PumpEnd::WidebandUndecodable);
            }
            drain_microphone(microphone, voice, &mut uplink, uplink_limit, peer);
            let read = match link.recv(&mut packet) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    tracing::info!(peer, %error, "Bluetooth call audio closed");
                    break;
                }
            };
            voice_link.receive(&packet[..read], &mut downlink);
            while downlink.len() >= frame_bytes {
                let pcm = voice_link.mixer_pcm(&downlink[..frame_bytes]);
                let submitted = clock
                    .frame(AudioStreamId::High, CALL_FORMAT, CALL_OUTPUT_RATE, 1, &pcm)
                    .map_err(|error| error.to_string())
                    .and_then(|frame| frames.try_submit(frame).map_err(|error| error.to_string()));
                if let Err(error) = submitted {
                    voice.far_end(&silence, Instant::now());
                    dropped_frames = dropped_frames.saturating_add(1);
                    if dropped_frames.is_power_of_two() {
                        tracing::warn!(peer, dropped_frames, %error, "dropping Bluetooth call audio");
                    }
                } else {
                    voice.far_end(&downlink[..frame_bytes], Instant::now());
                }
                downlink.drain(..frame_bytes);
            }
            let reply = voice_link.transmit(&mut uplink, read);
            let mtu = match link.mtu() {
                0 => read,
                mtu => mtu,
            };
            for chunk in reply.chunks(mtu) {
                match link.send(chunk) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => {
                        tracing::info!(peer, %error, "sending call microphone audio failed");
                        return (clock, PumpEnd::Closed);
                    }
                }
            }
        }
        (clock, PumpEnd::Closed)
    }

    fn drain_microphone(
        microphone: &mut Option<LinuxMicrophoneBackend>,
        voice: &mut CallVoiceProcessor,
        uplink: &mut VecDeque<u8>,
        uplink_limit: usize,
        peer: &str,
    ) {
        let Some(backend) = microphone.as_ref() else {
            return;
        };
        if let Ok(Some(error)) = backend.try_error() {
            tracing::warn!(
                peer,
                component = error.component,
                error = %error.message,
                "call microphone stopped; the caller hears silence"
            );
            if let Some(backend) = microphone.take() {
                let _ = backend.shutdown();
            }
            return;
        }
        while let Ok(Some(event)) = backend.try_event() {
            if let MicrophoneBackendEvent::Packet { packet, .. } = event {
                voice.near_end(&packet.encoded, Instant::now(), uplink);
            }
        }
        let excess = uplink.len().saturating_sub(uplink_limit);
        let excess = (excess + excess % 2).min(uplink.len());
        uplink.drain(..excess);
    }

    fn take_uplink(uplink: &mut VecDeque<u8>, length: usize) -> Vec<u8> {
        let mut reply = vec![0_u8; length];
        let available = uplink.len().min(length);
        for (slot, byte) in reply.iter_mut().zip(uplink.drain(..available)) {
            *slot = byte;
        }
        reply
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use super::*;

    fn samples(pcm: &[u8]) -> Vec<i16> {
        pcm.chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
            .collect()
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    #[test]
    fn upsampling_interpolates_across_chunk_boundaries() {
        let mut upsampler = Upsampler::default();
        assert_eq!(
            samples(&upsampler.double_rate(&pcm(&[100, 200]))),
            [50, 100, 150, 200]
        );
        assert_eq!(
            samples(&upsampler.double_rate(&pcm(&[-200, i16::MIN]))),
            [0, -200, -16_484, i16::MIN]
        );
    }

    fn energy(samples: &[i16]) -> f64 {
        samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / samples.len().max(1) as f64
    }

    fn assert_echo_removed_and_driver_kept(sample_rate: u32, echo_delay: Duration) {
        let rate = sample_rate as usize;
        let chunk = rate / 50;
        let echo_delay = rate * echo_delay.as_millis() as usize / 1_000;
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let far_end: Vec<i16> = (0..rate * 12)
            .map(|index| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let envelope = match index / (rate / 4) {
                    talk if talk >= 36 => 0.0,
                    talk if talk % 3 == 2 => 0.1,
                    _ => 1.0,
                };
                (f64::from((seed >> 48) as i16 / 4) * envelope) as i16
            })
            .collect();
        let echo: Vec<i16> = (0..far_end.len())
            .map(|index| {
                let tap = |delay: usize| {
                    index
                        .checked_sub(echo_delay + delay)
                        .map_or(0.0, |source| f64::from(far_end[source]))
                };
                (0.5 * tap(0) + 0.2 * tap(3) - 0.1 * tap(11)) as i16
            })
            .collect();
        let driver: Vec<i16> = (0..far_end.len())
            .map(|index| {
                if index < rate * 10 || index % (rate * 3 / 10) >= rate / 5 {
                    return 0;
                }
                let time = index as f64 / rate as f64;
                let voiced: f64 = (1..=12_u32)
                    .map(|harmonic| {
                        let harmonic = f64::from(harmonic);
                        (std::f64::consts::TAU * 140.0 * harmonic * time).sin() / harmonic
                    })
                    .sum();
                (voiced * 2_500.0) as i16
            })
            .collect();
        let microphone: Vec<i16> = echo
            .iter()
            .zip(&driver)
            .map(|(echo, driver)| echo.saturating_add(*driver))
            .collect();
        let mut voice = CallVoiceProcessor::new(sample_rate);
        let mut uplink = VecDeque::new();
        let started = Instant::now();
        for (index, (far, near)) in far_end
            .chunks(chunk)
            .zip(microphone.chunks(chunk))
            .enumerate()
        {
            let now = started + Duration::from_millis(20) * index as u32;
            voice.far_end(&pcm(far), now);
            voice.near_end(&pcm(near), now, &mut uplink);
        }
        let processed = samples(&uplink.into_iter().collect::<Vec<_>>());
        assert_eq!(processed.len(), microphone.len());
        let echo_only = rate * 4..rate * 9;
        let reduction_db =
            10.0 * (energy(&echo[echo_only.clone()]) / energy(&processed[echo_only])).log10();
        assert!(
            reduction_db > 20.0,
            "echo only reduced by {reduction_db:.1} dB"
        );
        let driver_only = rate * 21 / 2..rate * 12;
        let kept_db =
            10.0 * (energy(&processed[driver_only.clone()]) / energy(&driver[driver_only])).log10();
        assert!(
            kept_db > -6.0,
            "the driver was attenuated by {kept_db:.1} dB"
        );
    }

    #[test]
    fn call_voice_processing_removes_the_expected_car_speaker_echo() {
        for codec in [CallCodec::Cvsd, CallCodec::Msbc] {
            assert_echo_removed_and_driver_kept(voice_rate(codec), Duration::from_millis(400));
        }
    }

    #[test]
    fn call_voice_processing_reaches_echo_beyond_the_canceller_window() {
        for codec in [CallCodec::Cvsd, CallCodec::Msbc] {
            assert_echo_removed_and_driver_kept(voice_rate(codec), Duration::from_millis(550));
        }
    }

    fn distant_voice(sample_rate: u32, seconds: usize, amplitude: f64) -> Vec<i16> {
        let rate = f64::from(sample_rate);
        let vowels = [
            [700.0, 1_150.0, 2_500.0],
            [300.0, 2_200.0, 3_000.0],
            [500.0, 900.0, 2_400.0],
        ];
        let mut glottis = 0.0;
        let mut resonators = [[0.0_f64; 2]; 3];
        (0..seconds * sample_rate as usize)
            .map(|index| {
                let time = index as f64 / rate;
                glottis += (120.0 + 25.0 * (time * 1.3 * std::f64::consts::TAU).sin()) / rate;
                let pulse = if glottis >= 1.0 {
                    glottis -= 1.0;
                    1.0
                } else {
                    0.0
                };
                let syllable = (time * 4.0).fract() * std::f64::consts::PI;
                let vowel = vowels[(time * 4.0) as usize % vowels.len()];
                let mut sample = 0.0;
                for (formant, (frequency, state)) in vowel.iter().zip(&mut resonators).enumerate() {
                    let angle = std::f64::consts::TAU * frequency / rate;
                    let next = pulse + 2.0 * 0.97 * angle.cos() * state[0] - 0.97 * 0.97 * state[1];
                    state[1] = state[0];
                    state[0] = next;
                    sample += next / (formant + 1) as f64;
                }
                (sample * syllable.sin().powi(2) * amplitude) as i16
            })
            .collect()
    }

    #[test]
    fn call_voice_processing_brings_a_distant_talker_up_to_call_level() {
        for codec in [CallCodec::Cvsd, CallCodec::Msbc] {
            let sample_rate = voice_rate(codec);
            let rate = sample_rate as usize;
            let chunk = rate / 50;
            let talker = distant_voice(sample_rate, 10, 30.0);
            let mut voice = CallVoiceProcessor::new(sample_rate);
            let mut uplink = VecDeque::new();
            let started = Instant::now();
            for (index, near) in talker.chunks(chunk).enumerate() {
                let now = started + Duration::from_millis(20) * index as u32;
                voice.far_end(&vec![0; chunk * 2], now);
                voice.near_end(&pcm(near), now, &mut uplink);
            }
            let processed = samples(&uplink.into_iter().collect::<Vec<_>>());
            let level_dbfs = |samples: &[i16]| {
                10.0 * (energy(&samples[rate * 7..rate * 10]) / f64::from(i16::MAX).powi(2)).log10()
            };
            let before = level_dbfs(&talker);
            let after = level_dbfs(&processed);
            assert!(
                before < -50.0,
                "{codec:?}: the talker starts at {before:.1} dBFS"
            );
            assert!(
                (-32.0..-20.0).contains(&after),
                "{codec:?}: the talker reaches {after:.1} dBFS"
            );
        }
    }

    #[test]
    fn call_voice_processing_quiets_steady_hiss_without_quieting_the_talker() {
        for codec in [CallCodec::Cvsd, CallCodec::Msbc] {
            let sample_rate = voice_rate(codec);
            let rate = sample_rate as usize;
            let chunk = rate / 50;
            let mut seed = 0x2545_f491_u32;
            let mut talker = distant_voice(sample_rate, 8, 30.0);
            talker.resize(11 * rate, 0);
            let near_end: Vec<i16> = talker
                .iter()
                .map(|sample| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let hiss = f64::from(seed) / f64::from(u32::MAX) * 2.0 - 1.0;
                    (f64::from(*sample) + hiss * 8.0) as i16
                })
                .collect();
            let mut voice = CallVoiceProcessor::new(sample_rate);
            let mut uplink = VecDeque::new();
            let started = Instant::now();
            for (index, near) in near_end.chunks(chunk).enumerate() {
                let now = started + Duration::from_millis(20) * index as u32;
                voice.far_end(&vec![0; chunk * 2], now);
                voice.near_end(&pcm(near), now, &mut uplink);
            }
            let processed = samples(&uplink.into_iter().collect::<Vec<_>>());
            let talking = 5 * rate..8 * rate;
            let paused = 9 * rate..11 * rate;
            let hiss_below_db = |samples: &[i16]| {
                10.0 * (energy(&samples[talking.clone()]) / energy(&samples[paused.clone()]))
                    .log10()
            };
            let before = hiss_below_db(&near_end);
            let after = hiss_below_db(&processed);
            assert!(
                after - before > 6.0,
                "{codec:?}: the hiss went from {before:.1} to {after:.1} dB below the talker"
            );
            let talker_dbfs =
                10.0 * (energy(&processed[talking.clone()]) / f64::from(i16::MAX).powi(2)).log10();
            assert!(
                talker_dbfs > -32.0,
                "{codec:?}: the talker fell to {talker_dbfs:.1} dBFS"
            );
        }
    }

    #[test]
    fn call_voice_processing_waits_for_whole_frames() {
        for codec in [CallCodec::Cvsd, CallCodec::Msbc] {
            let frame_bytes = voice_rate(codec) as usize / 100 * 2;
            let mut voice = CallVoiceProcessor::new(voice_rate(codec));
            let mut uplink = VecDeque::new();
            let now = Instant::now();
            voice.far_end(&[0; 7], now);
            voice.near_end(&vec![0; frame_bytes - 1], now, &mut uplink);
            assert!(uplink.is_empty());
            voice.near_end(&vec![0; frame_bytes + 3], now, &mut uplink);
            assert_eq!(uplink.len(), frame_bytes * 2);
            voice.near_end(&[0; 1], now, &mut uplink);
            assert_eq!(uplink.len(), frame_bytes * 2);
        }
    }

    #[test]
    fn calls_use_the_codec_the_phone_selected_until_hd_audio_fails() {
        let directory = tempfile::TempDir::new().expect("state directory");
        let codecs = CallCodecs::new(directory.path());
        assert!(codecs.wideband());
        assert_eq!(codecs.selected("0c:c4:13:31:cb:4d"), CallCodec::Cvsd);
        codecs.select("0c:c4:13:31:cb:4d", CallCodec::Msbc);
        assert_eq!(codecs.selected("0C:C4:13:31:CB:4D"), CallCodec::Msbc);
        assert!(!codecs.take_narrowband_request("0C:C4:13:31:CB:4D"));

        codecs.fall_back_to_narrowband("0C:C4:13:31:CB:4D");
        assert!(!codecs.wideband());
        assert_eq!(codecs.selected("0c:c4:13:31:cb:4d"), CallCodec::Cvsd);
        assert!(codecs.take_narrowband_request("0c:c4:13:31:cb:4d"));
        assert!(!codecs.take_narrowband_request("0c:c4:13:31:cb:4d"));
        assert!(!CallCodecs::new(directory.path()).wideband());

        codecs.select("0C:C4:13:31:CB:4D", CallCodec::Msbc);
        codecs.forget("0c:c4:13:31:cb:4d");
        assert_eq!(codecs.selected("0C:C4:13:31:CB:4D"), CallCodec::Cvsd);
    }

    #[test]
    fn hd_calls_run_at_the_msbc_rate_on_a_transparent_link() {
        assert_eq!(voice_rate(CallCodec::Cvsd), 8_000);
        assert_eq!(voice_rate(CallCodec::Msbc), 16_000);
        assert_eq!(air_mode(CallCodec::Cvsd), AirMode::Cvsd);
        assert_eq!(air_mode(CallCodec::Msbc), AirMode::Transparent);
    }

    #[test]
    fn the_car_address_is_only_offered_while_bluetooth_runs() {
        let address = Arc::new(RwLock::new(Some([0x30, 0xe3, 0xd6, 0x03, 0x96, 0x1e])));
        let control = Arc::new(RwLock::new(None));
        let hands_free =
            HandsFree::new(address, control, PairingPasskeys::default(), PathBuf::new());
        assert_eq!(hands_free.car_address(), None);
    }

    #[test]
    fn a_dropped_subscription_releases_only_its_own_slot() {
        let hands_free = HandsFree::new(
            Arc::default(),
            Arc::default(),
            PairingPasskeys::default(),
            PathBuf::new(),
        );
        let first = hands_free.subscribe();
        let second = hands_free.subscribe();
        drop(first);
        assert!(
            hands_free
                .listeners
                .lock()
                .expect("listeners")
                .current
                .is_some()
        );
        drop(second);
        assert!(
            hands_free
                .listeners
                .lock()
                .expect("listeners")
                .current
                .is_none()
        );
    }

    #[test]
    fn waiting_for_bluetooth_stops_when_it_is_not_expected_or_cancelled() {
        let hands_free = HandsFree::new(
            Arc::default(),
            Arc::default(),
            PairingPasskeys::default(),
            PathBuf::new(),
        );
        let started = Instant::now();
        assert_eq!(
            hands_free.wait_for_car_address(Duration::from_secs(5), || true),
            None
        );
        hands_free.set_bluetooth_expected(false);
        assert_eq!(
            hands_free.wait_for_car_address(Duration::from_secs(5), || false),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn usb_projection_lasts_until_every_session_ends() {
        let hands_free = HandsFree::new(
            Arc::default(),
            Arc::default(),
            PairingPasskeys::default(),
            PathBuf::new(),
        );
        assert!(!hands_free.projecting_over_usb());
        let first = hands_free.usb_session();
        let second = hands_free.clone().usb_session();
        assert!(hands_free.projecting_over_usb());
        drop(first);
        assert!(hands_free.projecting_over_usb());
        drop(second);
        assert!(!hands_free.projecting_over_usb());
    }

    #[test]
    fn phone_addresses_are_normalized_before_the_bond_lookup() {
        assert_eq!(
            parse_address("f0:5c:77:12:34:56")
                .map(format_address)
                .as_deref(),
            Some("F0:5C:77:12:34:56")
        );
        for malformed in [
            "",
            "F0:5C:77:12:34",
            "F0:5C:77:12:34:56:78",
            "F0:5C:77:12:34:5",
            "zz:5C:77:12:34:56",
        ] {
            assert_eq!(parse_address(malformed), None);
        }
    }

    #[test]
    fn addresses_format_most_significant_first() {
        assert_eq!(
            format_address([0x30, 0xe3, 0xd6, 0x03, 0x96, 0x1e]),
            "30:E3:D6:03:96:1E"
        );
    }
}
