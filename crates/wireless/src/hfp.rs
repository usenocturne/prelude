use std::{
    collections::VecDeque,
    io::{self, Read, Write},
};

use crate::{Error, Result};

pub const HANDS_FREE_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";
pub const AUDIO_GATEWAY_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";

const HF_SUPPORTED_FEATURES: u32 = (1 << 2) | (1 << 3) | (1 << 4);
const HF_CODEC_NEGOTIATION: u32 = 1 << 7;
const SDP_SHARED_FEATURES: u32 = 0x1f;
const SDP_WIDE_BAND_SPEECH: u16 = 1 << 5;
const AG_ECHO_AND_NOISE_REDUCTION: u32 = 1 << 1;
const AG_CODEC_NEGOTIATION: u32 = 1 << 9;
const DISABLE_ECHO_AND_NOISE_REDUCTION: &str = "AT+NREC=0";
const MAX_LINE_LEN: usize = 512;

const POST_SLC_COMMANDS: [&str; 4] = ["AT+CLIP=1", "AT+CCWA=1", "AT+CMEE=1", "AT+CLCC"];

pub const fn sdp_features(wideband: bool) -> u16 {
    let shared = (HF_SUPPORTED_FEATURES & SDP_SHARED_FEATURES) as u16;
    if wideband {
        shared | SDP_WIDE_BAND_SPEECH
    } else {
        shared
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallCodec {
    Cvsd,
    Msbc,
}

impl CallCodec {
    const fn id(self) -> u8 {
        match self {
            Self::Cvsd => 1,
            Self::Msbc => 2,
        }
    }

    const fn from_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(Self::Cvsd),
            2 => Some(Self::Msbc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Features,
    Codecs,
    IndicatorMap,
    IndicatorValues,
    EventReporting,
    Established,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Awaiting {
    Connection,
    Command,
    PhoneVoiceProcessing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandsFreeEvent {
    Established,
    Battery { level: u8 },
    CodecSelected(CallCodec),
    PhoneEchoAndNoiseReduction { enabled: bool },
}

#[derive(Debug)]
pub struct HandsFree {
    step: Step,
    awaiting: Option<Awaiting>,
    wideband: bool,
    features: u32,
    gateway_features: u32,
    codecs_offered: bool,
    indicators: Vec<String>,
    call_active: bool,
    queued: VecDeque<String>,
    line: Vec<u8>,
}

impl Default for HandsFree {
    fn default() -> Self {
        Self::new(false)
    }
}

impl HandsFree {
    pub fn new(wideband: bool) -> Self {
        let features = if wideband {
            HF_SUPPORTED_FEATURES | HF_CODEC_NEGOTIATION
        } else {
            HF_SUPPORTED_FEATURES
        };
        Self {
            step: Step::Features,
            awaiting: None,
            wideband,
            features,
            gateway_features: 0,
            codecs_offered: false,
            indicators: Vec::new(),
            call_active: false,
            queued: VecDeque::new(),
            line: Vec::new(),
        }
    }

    pub const fn is_established(&self) -> bool {
        matches!(self.step, Step::Established)
    }

    pub fn start(&mut self) -> Vec<String> {
        if self.awaiting.is_some() || self.step != Step::Features {
            return Vec::new();
        }
        self.awaiting = Some(Awaiting::Connection);
        vec![command(&format!("AT+BRSF={}", self.features))]
    }

    pub fn fall_back_to_narrowband(&mut self) -> Vec<String> {
        if !self.wideband {
            return Vec::new();
        }
        self.wideband = false;
        if self.codecs_offered {
            if self.call_active && self.is_established() {
                self.queued.push_front("AT+BCC".to_owned());
            }
            self.queued.push_front(self.available_codecs());
        }
        let mut outgoing = Vec::new();
        self.send_queued(&mut outgoing);
        outgoing
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(Vec<String>, Vec<HandsFreeEvent>)> {
        let mut outgoing = Vec::new();
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                if !self.line.is_empty() {
                    let line = String::from_utf8_lossy(&self.line).trim().to_owned();
                    self.line.clear();
                    self.handle_line(&line, &mut outgoing, &mut events);
                }
                continue;
            }
            if self.line.len() >= MAX_LINE_LEN {
                return Err(Error::Protocol("HFP line exceeds the maximum length"));
            }
            self.line.push(byte);
        }
        Ok((outgoing, events))
    }

    fn handle_line(
        &mut self,
        line: &str,
        outgoing: &mut Vec<String>,
        events: &mut Vec<HandsFreeEvent>,
    ) {
        if line == "OK" || line == "ERROR" || line.starts_with("+CME ERROR") {
            match self.awaiting.take() {
                Some(Awaiting::Connection) => self.advance(outgoing, events),
                Some(Awaiting::PhoneVoiceProcessing) => {
                    events.push(HandsFreeEvent::PhoneEchoAndNoiseReduction {
                        enabled: line != "OK",
                    });
                }
                Some(Awaiting::Command) | None => {}
            }
            self.send_queued(outgoing);
            return;
        }
        if let Some(features) = line.strip_prefix("+BRSF:") {
            self.gateway_features = features.trim().parse().unwrap_or_default();
            return;
        }
        if let Some(codec) = line.strip_prefix("+BCS:") {
            self.select_codec(codec.trim(), events);
            self.send_queued(outgoing);
            return;
        }
        if let Some(values) = line.strip_prefix("+CIND:") {
            let values = values.trim();
            if values.starts_with('(') {
                self.indicators = values
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect();
            } else {
                self.report_indicators(values, events);
            }
            return;
        }
        if let Some(event) = line.strip_prefix("+CIEV:") {
            let mut fields = event.split(',').map(str::trim);
            if let (Some(Ok(index)), Some(Ok(value))) = (
                fields.next().map(str::parse::<usize>),
                fields.next().map(str::parse::<u8>),
            ) && let Some(name) = self.indicators.get(index.wrapping_sub(1)).cloned()
            {
                self.indicator_changed(&name, value, events);
            }
        }
    }

    fn select_codec(&mut self, id: &str, events: &mut Vec<HandsFreeEvent>) {
        let codec = id
            .parse()
            .ok()
            .and_then(CallCodec::from_id)
            .filter(|codec| self.supports(*codec));
        match codec {
            Some(codec) => {
                self.queued.push_front(format!("AT+BCS={}", codec.id()));
                events.push(HandsFreeEvent::CodecSelected(codec));
            }
            None => self.queued.push_front(self.available_codecs()),
        }
    }

    fn supports(&self, codec: CallCodec) -> bool {
        codec == CallCodec::Cvsd || self.wideband
    }

    fn available_codecs(&self) -> String {
        if self.wideband {
            "AT+BAC=1,2".to_owned()
        } else {
            "AT+BAC=1".to_owned()
        }
    }

    fn report_indicators(&mut self, values: &str, events: &mut Vec<HandsFreeEvent>) {
        let reported: Vec<(String, u8)> = self
            .indicators
            .iter()
            .zip(values.split(','))
            .filter_map(|(name, value)| Some((name.clone(), value.trim().parse().ok()?)))
            .collect();
        for (name, value) in reported {
            self.indicator_changed(&name, value, events);
        }
    }

    fn indicator_changed(&mut self, name: &str, value: u8, events: &mut Vec<HandsFreeEvent>) {
        match name {
            "battchg" => events.push(HandsFreeEvent::Battery { level: value }),
            "call" => self.call_active = value == 1,
            _ => {}
        }
    }

    fn advance(&mut self, outgoing: &mut Vec<String>, events: &mut Vec<HandsFreeEvent>) {
        let next = match self.step {
            Step::Features
                if self.features & HF_CODEC_NEGOTIATION != 0
                    && self.gateway_features & AG_CODEC_NEGOTIATION != 0 =>
            {
                self.step = Step::Codecs;
                self.codecs_offered = true;
                Some(self.available_codecs())
            }
            Step::Features | Step::Codecs => {
                self.step = Step::IndicatorMap;
                Some("AT+CIND=?".to_owned())
            }
            Step::IndicatorMap => {
                self.step = Step::IndicatorValues;
                Some("AT+CIND?".to_owned())
            }
            Step::IndicatorValues => {
                self.step = Step::EventReporting;
                Some("AT+CMER=3,0,0,1".to_owned())
            }
            Step::EventReporting => {
                self.step = Step::Established;
                events.push(HandsFreeEvent::Established);
                if self.gateway_features & AG_ECHO_AND_NOISE_REDUCTION != 0 {
                    self.queued
                        .push_back(DISABLE_ECHO_AND_NOISE_REDUCTION.to_owned());
                }
                self.queued
                    .extend(POST_SLC_COMMANDS.into_iter().map(str::to_owned));
                None
            }
            Step::Established => None,
        };
        if let Some(next) = next {
            self.awaiting = Some(Awaiting::Connection);
            outgoing.push(command(&next));
        }
    }

    fn send_queued(&mut self, outgoing: &mut Vec<String>) {
        if self.awaiting.is_some() || !self.is_established() {
            return;
        }
        if let Some(next) = self.queued.pop_front() {
            self.awaiting = Some(if next == DISABLE_ECHO_AND_NOISE_REDUCTION {
                Awaiting::PhoneVoiceProcessing
            } else {
                Awaiting::Command
            });
            outgoing.push(command(&next));
        }
    }
}

fn command(text: &str) -> String {
    format!("{text}\r")
}

pub fn run_hands_free<S: Read + Write>(
    stream: &mut S,
    wideband: bool,
    cancelled: impl Fn() -> bool,
    fall_back_to_narrowband: impl Fn() -> bool,
    mut on_event: impl FnMut(HandsFreeEvent),
) -> Result<()> {
    let mut hands_free = HandsFree::new(wideband);
    write_lines(stream, hands_free.start())?;
    let mut buffer = [0_u8; 512];
    while !cancelled() {
        if fall_back_to_narrowband() {
            write_lines(stream, hands_free.fall_back_to_narrowband())?;
        }
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
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
            Err(error) => return Err(Error::Io(error)),
        };
        let (outgoing, events) = hands_free.receive(&buffer[..read])?;
        events.into_iter().for_each(&mut on_event);
        write_lines(stream, outgoing)?;
    }
    Ok(())
}

fn write_lines<S: Write>(stream: &mut S, lines: Vec<String>) -> Result<()> {
    for line in lines {
        stream.write_all(line.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(hands_free: &mut HandsFree, text: &str) -> (Vec<String>, Vec<HandsFreeEvent>) {
        hands_free.receive(text.as_bytes()).expect("receive")
    }

    #[test]
    fn establishes_the_service_level_connection() {
        let mut hf = HandsFree::new(false);
        assert_eq!(hf.start(), vec!["AT+BRSF=28\r".to_owned()]);

        let (out, _) = feed(&mut hf, "\r\n+BRSF: 871\r\n\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);

        let (out, _) = feed(
            &mut hf,
            "\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3)),(\"service\",(0,1)),(\"signal\",(0-5)),(\"roam\",(0,1)),(\"battchg\",(0-5)),(\"callheld\",(0-2))\r\n\r\nOK\r\n",
        );
        assert_eq!(out, vec!["AT+CIND?\r".to_owned()]);

        let (out, events) = feed(&mut hf, "\r\n+CIND: 0,0,1,4,0,3,0\r\n\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CMER=3,0,0,1\r".to_owned()]);
        assert_eq!(events, vec![HandsFreeEvent::Battery { level: 3 }]);
        assert!(!hf.is_established());

        let (out, events) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(events, vec![HandsFreeEvent::Established]);
        assert_eq!(out, vec!["AT+NREC=0\r".to_owned()]);
        assert!(hf.is_established());

        let (out, events) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(
            events,
            vec![HandsFreeEvent::PhoneEchoAndNoiseReduction { enabled: false }]
        );
        assert_eq!(out, vec!["AT+CLIP=1\r".to_owned()]);

        for expected in ["AT+CCWA=1\r", "AT+CMEE=1\r", "AT+CLCC\r"] {
            let (out, _) = feed(&mut hf, "\r\nOK\r\n");
            assert_eq!(out, vec![expected.to_owned()]);
        }
        let (out, _) = feed(&mut hf, "\r\nOK\r\n");
        assert!(out.is_empty());
    }

    #[test]
    fn errors_still_advance_the_dialogue() {
        let mut hf = HandsFree::new(false);
        hf.start();
        let (out, _) = feed(&mut hf, "\r\nERROR\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);
    }

    #[test]
    fn battery_indicator_events_follow_the_announced_order() {
        let mut hf = HandsFree::new(false);
        hf.start();
        feed(&mut hf, "\r\nOK\r\n");
        feed(
            &mut hf,
            "\r\n+CIND: (\"service\",(0,1)),(\"battchg\",(0-5))\r\n",
        );
        let (_, events) = feed(&mut hf, "\r\n+CIEV: 2,5\r\n");
        assert_eq!(events, vec![HandsFreeEvent::Battery { level: 5 }]);
    }

    #[test]
    fn split_lines_are_reassembled() {
        let mut hf = HandsFree::new(false);
        hf.start();
        let (out, _) = feed(&mut hf, "\r\nO");
        assert!(out.is_empty());
        let (out, _) = feed(&mut hf, "K\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);
    }

    fn establish_wideband(hf: &mut HandsFree) {
        assert_eq!(hf.start(), vec!["AT+BRSF=156\r".to_owned()]);
        let (out, _) = feed(hf, "\r\n+BRSF: 3943\r\n\r\nOK\r\n");
        assert_eq!(out, vec!["AT+BAC=1,2\r".to_owned()]);
        let (out, _) = feed(hf, "\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);
        feed(
            hf,
            "\r\n+CIND: (\"service\",(0-1)),(\"call\",(0-1)),(\"callsetup\",(0-3))\r\n\r\nOK\r\n",
        );
        feed(hf, "\r\n+CIND: 1,0,0\r\n\r\nOK\r\n");
        let (out, events) = feed(hf, "\r\nOK\r\n");
        assert_eq!(events, vec![HandsFreeEvent::Established]);
        assert_eq!(out, vec!["AT+NREC=0\r".to_owned()]);
        for _ in 0..=POST_SLC_COMMANDS.len() {
            feed(hf, "\r\nOK\r\n");
        }
    }

    #[test]
    fn wideband_offers_msbc_when_the_phone_negotiates_codecs() {
        let mut hf = HandsFree::new(true);
        establish_wideband(&mut hf);
        let (out, events) = feed(&mut hf, "\r\n+BCS: 2\r\n");
        assert_eq!(out, vec!["AT+BCS=2\r".to_owned()]);
        assert_eq!(events, vec![HandsFreeEvent::CodecSelected(CallCodec::Msbc)]);
        feed(&mut hf, "\r\nOK\r\n");
        let (out, events) = feed(&mut hf, "\r\n+BCS: 1\r\n");
        assert_eq!(out, vec!["AT+BCS=1\r".to_owned()]);
        assert_eq!(events, vec![HandsFreeEvent::CodecSelected(CallCodec::Cvsd)]);
    }

    #[test]
    fn wideband_skips_codec_negotiation_with_a_phone_that_lacks_it() {
        let mut hf = HandsFree::new(true);
        hf.start();
        let (out, _) = feed(&mut hf, "\r\n+BRSF: 3567\r\n\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);
    }

    #[test]
    fn unsupported_codecs_are_answered_with_the_available_list() {
        let mut hf = HandsFree::new(true);
        establish_wideband(&mut hf);
        let (out, events) = feed(&mut hf, "\r\n+BCS: 3\r\n");
        assert_eq!(out, vec!["AT+BAC=1,2\r".to_owned()]);
        assert!(events.is_empty());
    }

    #[test]
    fn codec_selection_waits_for_the_outstanding_command() {
        let mut hf = HandsFree::new(true);
        hf.start();
        feed(&mut hf, "\r\n+BRSF: 3943\r\n\r\nOK\r\n");
        for reply in ["\r\nOK\r\n"; 4] {
            feed(&mut hf, reply);
        }
        assert!(hf.is_established());
        let (out, events) = feed(&mut hf, "\r\n+BCS: 2\r\n");
        assert!(out.is_empty());
        assert_eq!(events, vec![HandsFreeEvent::CodecSelected(CallCodec::Msbc)]);
        let (out, _) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(out, vec!["AT+BCS=2\r".to_owned()]);
        let (out, _) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CLIP=1\r".to_owned()]);
    }

    #[test]
    fn phone_echo_and_noise_reduction_stays_on_when_the_phone_refuses() {
        let mut hf = HandsFree::new(false);
        hf.start();
        for reply in ["\r\n+BRSF: 871\r\n\r\nOK\r\n", "\r\nOK\r\n", "\r\nOK\r\n"] {
            feed(&mut hf, reply);
        }
        let (out, _) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(out, vec!["AT+NREC=0\r".to_owned()]);
        let (out, events) = feed(&mut hf, "\r\n+CME ERROR: 4\r\n");
        assert_eq!(
            events,
            vec![HandsFreeEvent::PhoneEchoAndNoiseReduction { enabled: true }]
        );
        assert_eq!(out, vec!["AT+CLIP=1\r".to_owned()]);
    }

    #[test]
    fn phones_without_echo_and_noise_reduction_are_not_asked_to_disable_it() {
        let mut hf = HandsFree::new(false);
        hf.start();
        for reply in ["\r\n+BRSF: 869\r\n\r\nOK\r\n", "\r\nOK\r\n", "\r\nOK\r\n"] {
            feed(&mut hf, reply);
        }
        let (out, events) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(events, vec![HandsFreeEvent::Established]);
        assert_eq!(out, vec!["AT+CLIP=1\r".to_owned()]);
    }

    #[test]
    fn narrowband_fallback_reopens_the_call_audio_only_during_a_call() {
        let mut hf = HandsFree::new(true);
        establish_wideband(&mut hf);
        feed(&mut hf, "\r\n+CIEV: 2,1\r\n");
        assert_eq!(hf.fall_back_to_narrowband(), vec!["AT+BAC=1\r".to_owned()]);
        let (out, _) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(out, vec!["AT+BCC\r".to_owned()]);
        assert!(hf.fall_back_to_narrowband().is_empty());
        let (out, events) = feed(&mut hf, "\r\nOK\r\n\r\n+BCS: 2\r\n");
        assert_eq!(out, vec!["AT+BAC=1\r".to_owned()]);
        assert!(events.is_empty());

        let mut idle = HandsFree::new(true);
        establish_wideband(&mut idle);
        assert_eq!(
            idle.fall_back_to_narrowband(),
            vec!["AT+BAC=1\r".to_owned()]
        );
        let (out, _) = feed(&mut idle, "\r\nOK\r\n");
        assert!(out.is_empty());
    }

    #[test]
    fn narrowband_fallback_is_silent_without_codec_negotiation() {
        let mut hf = HandsFree::new(false);
        hf.start();
        feed(&mut hf, "\r\n+BRSF: 3943\r\n\r\nOK\r\n");
        assert!(hf.fall_back_to_narrowband().is_empty());
        let mut legacy_phone = HandsFree::new(true);
        legacy_phone.start();
        feed(&mut legacy_phone, "\r\n+BRSF: 3567\r\n\r\nOK\r\n");
        assert!(legacy_phone.fall_back_to_narrowband().is_empty());
    }

    #[test]
    fn the_service_record_mirrors_brsf_and_advertises_wide_band_speech_only_for_hd_calls() {
        assert_eq!(sdp_features(false), 0x1c);
        assert_eq!(sdp_features(true), 0x3c);
        assert_eq!(
            u32::from(sdp_features(true)) & SDP_SHARED_FEATURES,
            HandsFree::new(true).features & SDP_SHARED_FEATURES
        );
    }

    #[test]
    fn runaway_lines_are_rejected() {
        let mut hf = HandsFree::new(false);
        assert!(hf.receive(&[b'x'; MAX_LINE_LEN + 1]).is_err());
    }
}
