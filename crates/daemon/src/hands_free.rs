#![cfg_attr(
    not(all(target_os = "linux", feature = "linux-media")),
    allow(dead_code)
)]

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use prelude_bluetooth_sco::ScoLink;
use prelude_wireless::bluez::{PairingPasskeys, PhoneKind};
use sonora::{
    AudioProcessing, AudioProcessingStats, Config, StreamConfig,
    config::{EchoCanceller, HighPassFilter, NoiseSuppression},
};

use crate::wireless_runtime::{SharedBluetoothAddress, SharedBluetoothControl, remember_phone};

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub(crate) use bridge::CallBridge;

const CAR_ADDRESS_POLL: Duration = Duration::from_millis(100);
const CALL_VOICE_RATE: u32 = 8_000;
const CALL_VOICE_FRAME_SAMPLES: usize = CALL_VOICE_RATE as usize / 100;
const CALL_VOICE_FRAME_BYTES: usize = CALL_VOICE_FRAME_SAMPLES * 2;
const ECHO_REFERENCE_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
struct CallListeners {
    next_id: u64,
    current: Option<(u64, SyncSender<ScoLink>)>,
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
            state_dir,
            listeners: Arc::default(),
            usb_sessions: Arc::default(),
            expected: Arc::new(AtomicBool::new(true)),
        }
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

    pub(crate) fn offer_call(&self, link: ScoLink) {
        let peer = format_address(link.peer());
        let sender = self
            .listeners
            .lock()
            .ok()
            .and_then(|listeners| listeners.current.as_ref().map(|(_, sender)| sender.clone()));
        let refused = match sender {
            Some(sender) => match sender.try_send(link) {
                Ok(()) => None,
                Err(TrySendError::Full(_)) => Some("the previous call audio is still starting"),
                Err(TrySendError::Disconnected(_)) => Some("the Android Auto session ended"),
            },
            None => Some("no Android Auto session is playing audio on the DAC"),
        };
        match refused {
            None => tracing::info!(peer, "the phone opened Bluetooth call audio"),
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
    links: Receiver<ScoLink>,
}

impl CallSubscription {
    pub(crate) fn try_next(&self) -> Option<ScoLink> {
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
    scheduled: VecDeque<(Instant, Vec<u8>)>,
    far_end: Vec<u8>,
    near_end: Vec<u8>,
    input: Vec<i16>,
    output: Vec<i16>,
}

impl CallVoiceProcessor {
    pub(crate) fn new() -> Self {
        let stream = StreamConfig::new(CALL_VOICE_RATE, 1);
        let processing = AudioProcessing::builder()
            .config(Config {
                high_pass_filter: Some(HighPassFilter::default()),
                echo_canceller: Some(EchoCanceller::default()),
                noise_suppression: Some(NoiseSuppression::default()),
                ..Config::default()
            })
            .capture_config(stream)
            .render_config(stream)
            .build();
        Self {
            processing,
            scheduled: VecDeque::new(),
            far_end: Vec::with_capacity(CALL_VOICE_FRAME_BYTES * 4),
            near_end: Vec::with_capacity(CALL_VOICE_FRAME_BYTES * 4),
            input: vec![0; CALL_VOICE_FRAME_SAMPLES],
            output: vec![0; CALL_VOICE_FRAME_SAMPLES],
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
        let complete = self.far_end.len() - self.far_end.len() % CALL_VOICE_FRAME_BYTES;
        for frame in self.far_end[..complete].chunks_exact(CALL_VOICE_FRAME_BYTES) {
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
        let complete = self.near_end.len() - self.near_end.len() % CALL_VOICE_FRAME_BYTES;
        for frame in self.near_end[..complete].chunks_exact(CALL_VOICE_FRAME_BYTES) {
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

    use prelude_bluetooth_sco::ScoLink;
    use prelude_media::{
        AudioFormatId, AudioFrameSender, AudioStreamId, LinuxMicrophoneBackend,
        MicrophoneBackendEvent,
    };

    use super::{CallVoiceProcessor, Upsampler, format_address};
    use crate::android_auto_runtime::AudioClock;

    const CALL_FORMAT: AudioFormatId = AudioFormatId::new(0x10);
    const CALL_OUTPUT_RATE: u32 = 16_000;
    const DOWNLINK_FRAME_BYTES: usize = 320;
    const SILENT_DOWNLINK_FRAME: [u8; DOWNLINK_FRAME_BYTES] = [0; DOWNLINK_FRAME_BYTES];
    const UPLINK_LIMIT_BYTES: usize = 1_600;
    const READ_BUFFER_BYTES: usize = 1_024;
    const LINK_TIMEOUT: Duration = Duration::from_millis(200);

    #[derive(Debug)]
    pub(crate) struct CallBridge {
        peer: String,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<AudioClock>>,
    }

    impl CallBridge {
        pub(crate) fn start(
            link: ScoLink,
            frames: AudioFrameSender,
            clock: AudioClock,
            microphone: Option<LinuxMicrophoneBackend>,
        ) -> io::Result<Self> {
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
                        let mut voice = CallVoiceProcessor::new();
                        let clock = pump(
                            &link,
                            &frames,
                            clock,
                            &stop,
                            &mut microphone,
                            &mut voice,
                            &peer,
                        );
                        let statistics = voice.statistics();
                        tracing::info!(
                            peer,
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

    fn pump(
        link: &ScoLink,
        frames: &AudioFrameSender,
        mut clock: AudioClock,
        stop: &AtomicBool,
        microphone: &mut Option<LinuxMicrophoneBackend>,
        voice: &mut CallVoiceProcessor,
        peer: &str,
    ) -> AudioClock {
        let mut uplink = VecDeque::with_capacity(UPLINK_LIMIT_BYTES * 2);
        let mut packet = vec![0_u8; READ_BUFFER_BYTES];
        let mut downlink = Vec::with_capacity(DOWNLINK_FRAME_BYTES * 2);
        let mut upsampler = Upsampler::default();
        let mut dropped_frames = 0_u64;
        while !stop.load(Ordering::Acquire) {
            drain_microphone(microphone, voice, &mut uplink, peer);
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
            downlink.extend_from_slice(&packet[..read]);
            while downlink.len() >= DOWNLINK_FRAME_BYTES {
                let pcm = upsampler.double_rate(&downlink[..DOWNLINK_FRAME_BYTES]);
                let submitted = clock
                    .frame(AudioStreamId::High, CALL_FORMAT, CALL_OUTPUT_RATE, 1, &pcm)
                    .map_err(|error| error.to_string())
                    .and_then(|frame| frames.try_submit(frame).map_err(|error| error.to_string()));
                if let Err(error) = submitted {
                    voice.far_end(&SILENT_DOWNLINK_FRAME, Instant::now());
                    dropped_frames = dropped_frames.saturating_add(1);
                    if dropped_frames.is_power_of_two() {
                        tracing::warn!(peer, dropped_frames, %error, "dropping Bluetooth call audio");
                    }
                } else {
                    voice.far_end(&downlink[..DOWNLINK_FRAME_BYTES], Instant::now());
                }
                downlink.drain(..DOWNLINK_FRAME_BYTES);
            }
            let reply = take_uplink(&mut uplink, read);
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
                        return clock;
                    }
                }
            }
        }
        clock
    }

    fn drain_microphone(
        microphone: &mut Option<LinuxMicrophoneBackend>,
        voice: &mut CallVoiceProcessor,
        uplink: &mut VecDeque<u8>,
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
        let excess = uplink.len().saturating_sub(UPLINK_LIMIT_BYTES);
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

    fn assert_echo_removed_and_driver_kept(echo_delay: Duration) {
        const RATE: usize = CALL_VOICE_RATE as usize;
        const CHUNK: usize = RATE / 50;
        let echo_delay = RATE * echo_delay.as_millis() as usize / 1_000;
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let far_end: Vec<i16> = (0..RATE * 12)
            .map(|index| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let envelope = match index / (RATE / 4) {
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
                if index < RATE * 10 || index % (RATE * 3 / 10) >= RATE / 5 {
                    return 0;
                }
                let time = index as f64 / RATE as f64;
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
        let mut voice = CallVoiceProcessor::new();
        let mut uplink = VecDeque::new();
        let started = Instant::now();
        for (index, (far, near)) in far_end
            .chunks(CHUNK)
            .zip(microphone.chunks(CHUNK))
            .enumerate()
        {
            let now = started + Duration::from_millis(20) * index as u32;
            voice.far_end(&pcm(far), now);
            voice.near_end(&pcm(near), now, &mut uplink);
        }
        let processed = samples(&uplink.into_iter().collect::<Vec<_>>());
        assert_eq!(processed.len(), microphone.len());
        let echo_only = RATE * 4..RATE * 9;
        let reduction_db =
            10.0 * (energy(&echo[echo_only.clone()]) / energy(&processed[echo_only])).log10();
        assert!(
            reduction_db > 20.0,
            "echo only reduced by {reduction_db:.1} dB"
        );
        let driver_only = RATE * 21 / 2..RATE * 12;
        let kept_db =
            10.0 * (energy(&processed[driver_only.clone()]) / energy(&driver[driver_only])).log10();
        assert!(
            kept_db > -6.0,
            "the driver was attenuated by {kept_db:.1} dB"
        );
    }

    #[test]
    fn call_voice_processing_removes_the_expected_car_speaker_echo() {
        assert_echo_removed_and_driver_kept(Duration::from_millis(400));
    }

    #[test]
    fn call_voice_processing_reaches_echo_beyond_the_canceller_window() {
        assert_echo_removed_and_driver_kept(Duration::from_millis(550));
    }

    #[test]
    fn call_voice_processing_waits_for_whole_frames() {
        let mut voice = CallVoiceProcessor::new();
        let mut uplink = VecDeque::new();
        let now = Instant::now();
        voice.far_end(&[0; 7], now);
        voice.near_end(&[0; CALL_VOICE_FRAME_BYTES - 1], now, &mut uplink);
        assert!(uplink.is_empty());
        voice.near_end(&[0; CALL_VOICE_FRAME_BYTES + 3], now, &mut uplink);
        assert_eq!(uplink.len(), CALL_VOICE_FRAME_BYTES * 2);
        voice.near_end(&[0; 1], now, &mut uplink);
        assert_eq!(uplink.len(), CALL_VOICE_FRAME_BYTES * 2);
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
