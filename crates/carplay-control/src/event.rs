use crate::{
    Error, HidDeviceKind, KnobButton, KnobReport, MediaButton, Result, TOUCH_CONTACTS,
    TelephonyButton, TouchReport, TouchSample,
};

const EVENT_CONTENT_TYPE: &str = "application/x-apple-binary-plist";
const MAXIMUM_EVENT_BODY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonotonicTimestamp(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SiriAction {
    ButtonDown = 2,
    ButtonUp = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    Down,
    Move,
    Up,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AirPlayEventKind {
    Hid {
        device: HidDeviceKind,
        report: Vec<u8>,
    },
    Siri(SiriAction),
    NightMode(bool),
    ForceKeyFrame {
        display_uuid: String,
    },
    IapMessage {
        length: usize,
    },
    Command {
        stream_id: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AirPlayEvent {
    pub sequence: u32,
    pub monotonic_timestamp: MonotonicTimestamp,
    pub kind: AirPlayEventKind,
    pub body: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct EventChannelEncoder {
    sequence: u32,
    last_timestamp: Option<MonotonicTimestamp>,
}

impl EventChannelEncoder {
    pub const fn new(initial_sequence: u32) -> Self {
        Self {
            sequence: initial_sequence,
            last_timestamp: None,
        }
    }

    pub const fn sequence(&self) -> u32 {
        self.sequence
    }

    pub const fn last_timestamp(&self) -> Option<MonotonicTimestamp> {
        self.last_timestamp
    }

    pub fn encode_hid(
        &mut self,
        device: HidDeviceKind,
        report: &[u8],
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        validate_hid_report(device, report)?;
        let uid = format!("{:x}", device.uid());
        let body = encode_bplist(BplistValue::Dictionary(vec![
            ("type", BplistValue::String("hidSendReport")),
            ("uuid", BplistValue::String(&uid)),
            ("hidReport", BplistValue::Data(report)),
        ]))?;
        self.finish(
            AirPlayEventKind::Hid {
                device,
                report: report.to_vec(),
            },
            body,
            timestamp,
        )
    }

    pub fn encode_siri(
        &mut self,
        action: SiriAction,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        let body = encode_bplist(BplistValue::Dictionary(vec![
            ("type", BplistValue::String("requestSiri")),
            (
                "params",
                BplistValue::Dictionary(vec![(
                    "siriAction",
                    BplistValue::Unsigned(u64::from(action as u8)),
                )]),
            ),
        ]))?;
        self.finish(AirPlayEventKind::Siri(action), body, timestamp)
    }

    pub fn encode_night_mode(
        &mut self,
        night: bool,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        let body = encode_bplist(BplistValue::Dictionary(vec![
            ("type", BplistValue::String("setNightMode")),
            (
                "params",
                BplistValue::Dictionary(vec![("nightMode", BplistValue::Boolean(night))]),
            ),
        ]))?;
        self.finish(AirPlayEventKind::NightMode(night), body, timestamp)
    }

    pub fn encode_force_key_frame(
        &mut self,
        display_uuid: &str,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        if display_uuid.is_empty()
            || display_uuid.len() > 128
            || !display_uuid.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(Error::MalformedWire(
                "CarPlay display UUID is malformed".to_owned(),
            ));
        }
        let body = encode_bplist(BplistValue::Dictionary(vec![
            ("type", BplistValue::String("forceKeyFrame")),
            (
                "params",
                BplistValue::Dictionary(vec![("uuid", BplistValue::String(display_uuid))]),
            ),
        ]))?;
        self.finish(
            AirPlayEventKind::ForceKeyFrame {
                display_uuid: display_uuid.to_owned(),
            },
            body,
            timestamp,
        )
    }

    pub fn encode_iap_message(
        &mut self,
        data: &[u8],
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        if data.is_empty() {
            return Err(Error::MalformedWire(
                "iAP2 tunnel message is empty".to_owned(),
            ));
        }
        let body = encode_bplist(BplistValue::Dictionary(vec![
            ("type", BplistValue::String("iAPSendMessage")),
            (
                "params",
                BplistValue::Dictionary(vec![("data", BplistValue::Data(data))]),
            ),
        ]))?;
        self.finish(
            AirPlayEventKind::IapMessage { length: data.len() },
            body,
            timestamp,
        )
    }

    pub fn encode_command(
        &mut self,
        body: &plist::Value,
        stream_id: Option<u64>,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        let typed = body
            .as_dictionary()
            .and_then(|dictionary| dictionary.get("type"))
            .and_then(plist::Value::as_string)
            .is_some_and(|name| !name.is_empty());
        if !typed && stream_id.is_none() {
            return Err(Error::MalformedWire(
                "an untyped event command must name its stream".to_owned(),
            ));
        }
        let mut encoded = Vec::new();
        body.to_writer_binary(&mut encoded)
            .map_err(|error| Error::MalformedWire(format!("event plist failed: {error}")))?;
        let headers = stream_id
            .map(|stream_id| format!("X-Apple-StreamID: {stream_id}\r\n"))
            .unwrap_or_default();
        self.finish_with_headers(
            AirPlayEventKind::Command { stream_id },
            encoded,
            &headers,
            timestamp,
        )
    }

    fn finish(
        &mut self,
        kind: AirPlayEventKind,
        body: Vec<u8>,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.finish_with_headers(kind, body, "", timestamp)
    }

    fn finish_with_headers(
        &mut self,
        kind: AirPlayEventKind,
        body: Vec<u8>,
        headers: &str,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        if let Some(previous) = self.last_timestamp {
            if timestamp < previous {
                return Err(Error::NonMonotonicTimestamp {
                    previous: previous.0,
                    received: timestamp.0,
                });
            }
        }
        if body.len() > MAXIMUM_EVENT_BODY {
            return Err(Error::MessageTooLarge {
                section: "event body",
                actual: body.len(),
                maximum: MAXIMUM_EVENT_BODY,
            });
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let header = format!(
            "POST /command RTSP/1.0\r\nContent-Type: {EVENT_CONTENT_TYPE}\r\nContent-Length: {}\r\nCSeq: {sequence}\r\n{headers}\r\n",
            body.len()
        );
        let mut message = Vec::with_capacity(header.len() + body.len());
        message.extend_from_slice(header.as_bytes());
        message.extend_from_slice(&body);
        self.sequence = sequence;
        self.last_timestamp = Some(timestamp);
        Ok(AirPlayEvent {
            sequence,
            monotonic_timestamp: timestamp,
            kind,
            body,
            message,
        })
    }
}

fn validate_hid_report(device: HidDeviceKind, report: &[u8]) -> Result<()> {
    match device {
        HidDeviceKind::Touch => {
            TouchReport::decode(report)?;
        }
        HidDeviceKind::Knob => {
            KnobReport::decode(report)?;
        }
        HidDeviceKind::Media => match report {
            [0] => {}
            [index] => {
                MediaButton::try_from(*index)?;
            }
            _ => {
                return Err(Error::MalformedWire(format!(
                    "media report length is {}, expected 1",
                    report.len()
                )));
            }
        },
        HidDeviceKind::Telephony => match report {
            [0] => {}
            [index] => {
                TelephonyButton::try_from(*index)?;
            }
            _ => {
                return Err(Error::MalformedWire(format!(
                    "telephony report length is {}, expected 1",
                    report.len()
                )));
            }
        },
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct InputWireSession {
    encoder: EventChannelEncoder,
    touches: [Option<TouchSample>; TOUCH_CONTACTS],
    knob_buttons: [bool; 3],
    media_button: Option<MediaButton>,
    telephony_button: Option<TelephonyButton>,
    siri_down: bool,
    torn_down: bool,
}

impl Default for InputWireSession {
    fn default() -> Self {
        Self::new()
    }
}

impl InputWireSession {
    pub const fn new() -> Self {
        Self {
            encoder: EventChannelEncoder::new(0),
            touches: [None; TOUCH_CONTACTS],
            knob_buttons: [false; 3],
            media_button: None,
            telephony_button: None,
            siri_down: false,
            torn_down: false,
        }
    }

    pub const fn event_sequence(&self) -> u32 {
        self.encoder.sequence()
    }

    pub const fn is_torn_down(&self) -> bool {
        self.torn_down
    }

    pub fn touch(
        &mut self,
        contact_id: u8,
        action: TouchAction,
        x: u16,
        y: u16,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        let index = usize::from(contact_id);
        if index >= TOUCH_CONTACTS {
            return Err(Error::InvalidInput(format!(
                "touch contact id {contact_id} is outside 0..{}",
                TOUCH_CONTACTS - 1
            )));
        }
        let sample = TouchSample {
            down: action != TouchAction::Up,
            x,
            y,
        };
        TouchReport {
            contacts: [sample, TouchSample::default()],
        }
        .encode()?;
        match (action, self.touches[index]) {
            (TouchAction::Down, Some(_)) => {
                return Err(Error::InvalidState("duplicate touch down"));
            }
            (TouchAction::Move | TouchAction::Up, None) => {
                return Err(Error::InvalidState("touch move/up without touch down"));
            }
            _ => {}
        }

        let mut next_touches = self.touches;
        next_touches[index] = Some(sample);
        let report = touch_state_report(next_touches).encode()?;
        let event = self
            .encoder
            .encode_hid(HidDeviceKind::Touch, &report, timestamp)?;
        if action == TouchAction::Up {
            next_touches[index] = None;
        }
        self.touches = next_touches;
        Ok(event)
    }

    pub fn knob_button(
        &mut self,
        button: KnobButton,
        pressed: bool,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        let index = knob_button_index(button);
        if self.knob_buttons[index] == pressed {
            return Err(Error::InvalidState(if pressed {
                "duplicate knob button press"
            } else {
                "knob button release without press"
            }));
        }
        let mut next_buttons = self.knob_buttons;
        next_buttons[index] = pressed;
        let report = knob_state_report(next_buttons, 0).encode()?;
        let event = self
            .encoder
            .encode_hid(HidDeviceKind::Knob, &report, timestamp)?;
        self.knob_buttons = next_buttons;
        Ok(event)
    }

    pub fn rotate(&mut self, relative: i32, timestamp: MonotonicTimestamp) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        if relative == 0 {
            return Err(Error::InvalidInput(
                "rotary relative movement cannot be zero".to_owned(),
            ));
        }
        let wheel = relative.clamp(-127, 127) as i8;
        let report = knob_state_report(self.knob_buttons, wheel).encode()?;
        self.encoder
            .encode_hid(HidDeviceKind::Knob, &report, timestamp)
    }

    pub fn media_button(
        &mut self,
        button: MediaButton,
        pressed: bool,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        match (pressed, self.media_button) {
            (true, None) => {}
            (true, Some(_)) => {
                return Err(Error::InvalidState("duplicate/overlapping media press"));
            }
            (false, Some(active)) if active == button => {}
            (false, _) => return Err(Error::InvalidState("media release without matching press")),
        }
        let report = [if pressed { button as u8 } else { 0 }];
        let event = self
            .encoder
            .encode_hid(HidDeviceKind::Media, &report, timestamp)?;
        self.media_button = pressed.then_some(button);
        Ok(event)
    }

    pub fn telephony_button(
        &mut self,
        button: TelephonyButton,
        pressed: bool,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        match (pressed, self.telephony_button) {
            (true, None) => {}
            (true, Some(_)) => {
                return Err(Error::InvalidState("duplicate/overlapping telephony press"));
            }
            (false, Some(active)) if active == button => {}
            (false, _) => {
                return Err(Error::InvalidState(
                    "telephony release without matching press",
                ));
            }
        }
        let report = [if pressed { button as u8 } else { 0 }];
        let event = self
            .encoder
            .encode_hid(HidDeviceKind::Telephony, &report, timestamp)?;
        self.telephony_button = pressed.then_some(button);
        Ok(event)
    }

    pub fn siri_button(
        &mut self,
        pressed: bool,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        if self.siri_down == pressed {
            return Err(Error::InvalidState(if pressed {
                "duplicate Siri button press"
            } else {
                "Siri button release without press"
            }));
        }
        let action = if pressed {
            SiriAction::ButtonDown
        } else {
            SiriAction::ButtonUp
        };
        let event = self.encoder.encode_siri(action, timestamp)?;
        self.siri_down = pressed;
        Ok(event)
    }

    pub fn cancel_siri(&mut self, timestamp: MonotonicTimestamp) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        let event = self.encoder.encode_siri(SiriAction::ButtonUp, timestamp)?;
        self.siri_down = false;
        Ok(event)
    }

    pub fn set_night_mode(
        &mut self,
        night: bool,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        self.encoder.encode_night_mode(night, timestamp)
    }

    pub fn force_key_frame(
        &mut self,
        display_uuid: &str,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        self.encoder.encode_force_key_frame(display_uuid, timestamp)
    }

    pub fn iap_message(
        &mut self,
        data: &[u8],
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        self.encoder.encode_iap_message(data, timestamp)
    }

    pub fn command(
        &mut self,
        body: &plist::Value,
        stream_id: Option<u64>,
        timestamp: MonotonicTimestamp,
    ) -> Result<AirPlayEvent> {
        self.ensure_live()?;
        self.encoder.encode_command(body, stream_id, timestamp)
    }

    pub fn release_all(&mut self, timestamp: MonotonicTimestamp) -> Result<Vec<AirPlayEvent>> {
        self.ensure_live()?;
        let mut next_encoder = self.encoder.clone();
        let mut events = Vec::new();
        if self.touches.iter().any(Option::is_some) {
            let contacts = self.touches.map(|contact| {
                contact.map_or_else(TouchSample::default, |sample| TouchSample {
                    down: false,
                    ..sample
                })
            });
            let report = TouchReport { contacts }.encode()?;
            events.push(next_encoder.encode_hid(HidDeviceKind::Touch, &report, timestamp)?);
        }
        if self.knob_buttons.contains(&true) {
            events.push(next_encoder.encode_hid(
                HidDeviceKind::Knob,
                &KnobReport::default().encode()?,
                timestamp,
            )?);
        }
        if self.media_button.is_some() {
            events.push(next_encoder.encode_hid(HidDeviceKind::Media, &[0], timestamp)?);
        }
        if self.telephony_button.is_some() {
            events.push(next_encoder.encode_hid(HidDeviceKind::Telephony, &[0], timestamp)?);
        }
        if self.siri_down {
            events.push(next_encoder.encode_siri(SiriAction::ButtonUp, timestamp)?);
        }
        self.encoder = next_encoder;
        self.touches = [None; TOUCH_CONTACTS];
        self.knob_buttons = [false; 3];
        self.media_button = None;
        self.telephony_button = None;
        self.siri_down = false;
        Ok(events)
    }

    pub fn teardown(&mut self, timestamp: MonotonicTimestamp) -> Result<Vec<AirPlayEvent>> {
        let events = self.release_all(timestamp)?;
        self.torn_down = true;
        Ok(events)
    }

    fn ensure_live(&self) -> Result<()> {
        if self.torn_down {
            Err(Error::TornDown)
        } else {
            Ok(())
        }
    }
}

fn touch_state_report(touches: [Option<TouchSample>; TOUCH_CONTACTS]) -> TouchReport {
    let contacts = touches.map(|contact| contact.unwrap_or_default());
    TouchReport { contacts }
}

const fn knob_button_index(button: KnobButton) -> usize {
    match button {
        KnobButton::Select => 0,
        KnobButton::Home => 1,
        KnobButton::Back => 2,
    }
}

const fn knob_state_report(buttons: [bool; 3], wheel: i8) -> KnobReport {
    KnobReport {
        select: buttons[0],
        home: buttons[1],
        back: buttons[2],
        x: 0,
        y: 0,
        wheel,
    }
}

#[derive(Debug)]
enum BplistValue<'a> {
    String(&'a str),
    Data(&'a [u8]),
    Unsigned(u64),
    Boolean(bool),
    Dictionary(Vec<(&'a str, BplistValue<'a>)>),
}

#[derive(Debug)]
enum EncodedNode {
    Leaf(Vec<u8>),
    Container {
        head: Vec<u8>,
        references: Vec<usize>,
    },
}

fn encode_bplist(root: BplistValue<'_>) -> Result<Vec<u8>> {
    let mut nodes = Vec::new();
    let top_index = add_bplist_node(&mut nodes, root)?;
    let reference_size = sized_width(nodes.len());
    let serialized = nodes
        .iter()
        .map(|node| match node {
            EncodedNode::Leaf(bytes) => Ok(bytes.clone()),
            EncodedNode::Container { head, references } => {
                let mut bytes = head.clone();
                for reference in references {
                    write_be(&mut bytes, *reference as u64, reference_size)?;
                }
                Ok(bytes)
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let mut encoded = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(serialized.len());
    for object in serialized {
        offsets.push(encoded.len());
        encoded.extend_from_slice(&object);
    }
    let offset_table_offset = encoded.len();
    let offset_size = sized_width(offset_table_offset);
    for offset in &offsets {
        write_be(&mut encoded, *offset as u64, offset_size)?;
    }
    let mut trailer = [0_u8; 32];
    trailer[6] = offset_size as u8;
    trailer[7] = reference_size as u8;
    trailer[8..16].copy_from_slice(&(nodes.len() as u64).to_be_bytes());
    trailer[16..24].copy_from_slice(&(top_index as u64).to_be_bytes());
    trailer[24..32].copy_from_slice(&(offset_table_offset as u64).to_be_bytes());
    encoded.extend_from_slice(&trailer);
    Ok(encoded)
}

fn add_bplist_node(nodes: &mut Vec<EncodedNode>, value: BplistValue<'_>) -> Result<usize> {
    let index = nodes.len();
    nodes.push(EncodedNode::Leaf(Vec::new()));
    nodes[index] = match value {
        BplistValue::String(value) => {
            if !value.is_ascii() {
                return Err(Error::MalformedWire(
                    "event plist contains a non-ASCII protocol string".to_owned(),
                ));
            }
            let mut body = marker(0x5, value.len())?;
            body.extend_from_slice(value.as_bytes());
            EncodedNode::Leaf(body)
        }
        BplistValue::Data(value) => {
            let mut body = marker(0x4, value.len())?;
            body.extend_from_slice(value);
            EncodedNode::Leaf(body)
        }
        BplistValue::Unsigned(value) => EncodedNode::Leaf(encoded_integer(value)),
        BplistValue::Boolean(value) => EncodedNode::Leaf(vec![if value { 0x09 } else { 0x08 }]),
        BplistValue::Dictionary(values) => {
            let mut references = Vec::with_capacity(values.len() * 2);
            for (key, _) in &values {
                references.push(add_bplist_node(nodes, BplistValue::String(key))?);
            }
            for (_, value) in values {
                references.push(add_bplist_node(nodes, value)?);
            }
            EncodedNode::Container {
                head: marker(0xd, references.len() / 2)?,
                references,
            }
        }
    };
    Ok(index)
}

fn marker(kind: u8, count: usize) -> Result<Vec<u8>> {
    if count < 15 {
        return Ok(vec![(kind << 4) | count as u8]);
    }
    let count = u64::try_from(count)
        .map_err(|_| Error::MalformedWire("plist object is too large".to_owned()))?;
    let mut bytes = vec![(kind << 4) | 0x0f];
    bytes.extend_from_slice(&encoded_integer(count));
    Ok(bytes)
}

fn encoded_integer(value: u64) -> Vec<u8> {
    let width: usize = if value > u64::from(u32::MAX) {
        8
    } else if value > u64::from(u16::MAX) {
        4
    } else if value > u64::from(u8::MAX) {
        2
    } else {
        1
    };
    let mut encoded = vec![0x10 | width.trailing_zeros() as u8];
    let be = value.to_be_bytes();
    encoded.extend_from_slice(&be[8 - width..]);
    encoded
}

const fn sized_width(value: usize) -> usize {
    if value > u16::MAX as usize {
        4
    } else if value > u8::MAX as usize {
        2
    } else {
        1
    }
}

fn write_be(target: &mut Vec<u8>, value: u64, width: usize) -> Result<()> {
    if !matches!(width, 1 | 2 | 4 | 8) || (width < 8 && value >= (1_u64 << (width * 8))) {
        return Err(Error::MalformedWire(
            "binary plist offset/reference does not fit".to_owned(),
        ));
    }
    let be = value.to_be_bytes();
    target.extend_from_slice(&be[8 - width..]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_commands_carry_the_stream_header() {
        let mut encoder = EventChannelEncoder::default();
        let mut body = plist::Dictionary::new();
        body.insert(
            "params".to_owned(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        );
        let event = encoder
            .encode_command(
                &plist::Value::Dictionary(body.clone()),
                Some(3),
                MonotonicTimestamp(1),
            )
            .expect("stream command");
        let text = String::from_utf8_lossy(&event.message);
        assert!(text.starts_with("POST /command RTSP/1.0\r\n"));
        assert!(text.contains("CSeq: 1\r\nX-Apple-StreamID: 3\r\n\r\nbplist00"));
        assert_eq!(event.kind, AirPlayEventKind::Command { stream_id: Some(3) });
        assert!(
            encoder
                .encode_command(&plist::Value::Dictionary(body), None, MonotonicTimestamp(2))
                .is_err()
        );
    }

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    #[test]
    fn hid_and_siri_bodies_match_livi_goldens() {
        let mut encoder = EventChannelEncoder::default();
        let touch = TouchReport {
            contacts: [
                TouchSample {
                    down: true,
                    x: 800,
                    y: 480,
                },
                TouchSample::default(),
            ],
        }
        .encode()
        .expect("touch");
        let event = encoder
            .encode_hid(HidDeviceKind::Touch, &touch, MonotonicTimestamp(1))
            .expect("event");
        assert_eq!(
            hex(&event.body),
            "62706c6973743030d301020304050654747970655475756964596869645265706f72745d68696453656e645265706f72745832613261326132614c00012003e001010000000000080f141923313a0000000000000101000000000000000700000000000000000000000000000047"
        );
        let knob = KnobReport {
            select: true,
            wheel: -1,
            ..KnobReport::default()
        }
        .encode()
        .expect("knob");
        let knob = encoder
            .encode_hid(HidDeviceKind::Knob, &knob, MonotonicTimestamp(2))
            .expect("knob event");
        assert_eq!(
            hex(&knob.body),
            "62706c6973743030d301020304050654747970655475756964596869645265706f72745d68696453656e645265706f727458326132613261326244010000ff080f141923313a000000000000010100000000000000070000000000000000000000000000003f"
        );
        let media = encoder
            .encode_hid(
                HidDeviceKind::Media,
                &[MediaButton::PlayPause as u8],
                MonotonicTimestamp(3),
            )
            .expect("media event");
        assert_eq!(
            hex(&media.body),
            "62706c6973743030d301020304050654747970655475756964596869645265706f72745d68696453656e645265706f72745832613261326132634103080f141923313a000000000000010100000000000000070000000000000000000000000000003c"
        );
        let telephony = encoder
            .encode_hid(
                HidDeviceKind::Telephony,
                &[TelephonyButton::HookSwitch as u8],
                MonotonicTimestamp(4),
            )
            .expect("telephony event");
        assert_eq!(
            hex(&telephony.body),
            "62706c6973743030d301020304050654747970655475756964596869645265706f72745d68696453656e645265706f72745832613261326132644101080f141923313a000000000000010100000000000000070000000000000000000000000000003c"
        );
        let siri = encoder
            .encode_siri(SiriAction::ButtonDown, MonotonicTimestamp(5))
            .expect("siri");
        assert_eq!(
            hex(&siri.body),
            "62706c6973743030d201020304547479706556706172616d735b7265717565737453697269d105065a73697269416374696f6e1002080d12192528330000000000000101000000000000000700000000000000000000000000000035"
        );
        let night_mode = encoder
            .encode_night_mode(false, MonotonicTimestamp(6))
            .expect("night mode");
        assert_eq!(
            hex(&night_mode.body),
            "62706c6973743030d201020304547479706556706172616d735c7365744e696768744d6f6465d10506596e696768744d6f646508080d12192629330000000000000101000000000000000700000000000000000000000000000034"
        );
        assert_eq!(night_mode.kind, AirPlayEventKind::NightMode(false));
    }

    #[test]
    fn event_request_has_exact_envelope_and_monotonic_sequence() {
        let mut encoder = EventChannelEncoder::default();
        let report = [MediaButton::PlayPause as u8];
        let first = encoder
            .encode_hid(HidDeviceKind::Media, &report, MonotonicTimestamp(10))
            .expect("first");
        assert_eq!(first.sequence, 1);
        assert!(first.message.starts_with(
            format!("POST /command RTSP/1.0\r\nContent-Type: {EVENT_CONTENT_TYPE}\r\nContent-Length: {}\r\nCSeq: 1\r\n\r\n", first.body.len()).as_bytes()
        ));
        let second = encoder
            .encode_hid(HidDeviceKind::Media, &[0], MonotonicTimestamp(10))
            .expect("same timestamp is monotonic");
        assert_eq!(second.sequence, 2);
        assert!(matches!(
            encoder.encode_hid(HidDeviceKind::Media, &[0], MonotonicTimestamp(9)),
            Err(Error::NonMonotonicTimestamp { .. })
        ));
    }

    #[test]
    fn force_keyframe_uses_the_authenticated_command_channel() {
        let display = "b7e6c5a0-1111-4000-8000-000000000001";
        let event = EventChannelEncoder::default()
            .encode_force_key_frame(display, MonotonicTimestamp(1))
            .expect("force keyframe");
        assert_eq!(
            event.kind,
            AirPlayEventKind::ForceKeyFrame {
                display_uuid: display.to_owned()
            }
        );
        assert!(event.body.starts_with(b"bplist00"));
        assert!(event.message.starts_with(b"POST /command RTSP/1.0\r\n"));
        assert!(
            EventChannelEncoder::default()
                .encode_force_key_frame("bad uuid\n", MonotonicTimestamp(1))
                .is_err()
        );
    }

    #[test]
    fn iap_messages_carry_raw_bytes_in_an_iap_send_message_command() {
        let event = EventChannelEncoder::default()
            .encode_iap_message(&[0xff, 0x5a, 0x00, 0x09], MonotonicTimestamp(1))
            .expect("iAP message");
        assert_eq!(event.kind, AirPlayEventKind::IapMessage { length: 4 });
        assert!(event.message.starts_with(b"POST /command RTSP/1.0\r\n"));
        let plist::Value::Dictionary(body) =
            plist::Value::from_reader(std::io::Cursor::new(&event.body)).expect("plist")
        else {
            panic!("command body is not a dictionary");
        };
        assert_eq!(
            body.get("type").and_then(plist::Value::as_string),
            Some("iAPSendMessage")
        );
        assert_eq!(
            body.get("params")
                .and_then(plist::Value::as_dictionary)
                .and_then(|params| params.get("data"))
                .and_then(plist::Value::as_data),
            Some(&[0xff, 0x5a, 0x00, 0x09][..])
        );
        assert!(
            EventChannelEncoder::default()
                .encode_iap_message(&[], MonotonicTimestamp(1))
                .is_err()
        );
    }

    #[test]
    fn malformed_report_and_sequence_exhaustion_fail_closed() {
        let mut encoder = EventChannelEncoder::default();
        assert!(
            encoder
                .encode_hid(HidDeviceKind::Media, &[7], MonotonicTimestamp(1))
                .is_err()
        );
        assert_eq!(encoder.sequence(), 0);
        let mut encoder = EventChannelEncoder::new(u32::MAX);
        assert!(matches!(
            encoder.encode_hid(HidDeviceKind::Media, &[0], MonotonicTimestamp(1)),
            Err(Error::SequenceExhausted)
        ));
    }

    #[test]
    fn touch_state_rejects_invalid_transitions_and_accepts_edges() {
        let mut session = InputWireSession::new();
        assert!(matches!(
            session.touch(0, TouchAction::Move, 0, 0, MonotonicTimestamp(1)),
            Err(Error::InvalidState(_))
        ));
        session
            .touch(0, TouchAction::Down, 0, 0, MonotonicTimestamp(1))
            .expect("down");
        assert!(
            session
                .touch(0, TouchAction::Down, 0, 0, MonotonicTimestamp(2))
                .is_err()
        );
        session
            .touch(0, TouchAction::Move, 800, 480, MonotonicTimestamp(3))
            .expect("edge move");
        session
            .touch(0, TouchAction::Up, 800, 480, MonotonicTimestamp(4))
            .expect("up");
        assert!(
            session
                .touch(0, TouchAction::Move, 1, 1, MonotonicTimestamp(5))
                .is_err()
        );
        assert!(
            session
                .touch(2, TouchAction::Down, 1, 1, MonotonicTimestamp(5))
                .is_err()
        );
        assert!(
            session
                .touch(1, TouchAction::Down, 801, 1, MonotonicTimestamp(5))
                .is_err()
        );
    }

    #[test]
    fn knob_media_and_siri_are_strict() {
        let mut session = InputWireSession::new();
        session
            .knob_button(KnobButton::Home, true, MonotonicTimestamp(1))
            .expect("home down");
        assert!(
            session
                .knob_button(KnobButton::Home, true, MonotonicTimestamp(2))
                .is_err()
        );
        let rotation = session
            .rotate(i32::MAX, MonotonicTimestamp(2))
            .expect("rotation");
        let AirPlayEventKind::Hid { report, .. } = rotation.kind else {
            panic!("HID event")
        };
        assert_eq!(i8::from_ne_bytes([report[3]]), 127);
        assert!(session.rotate(0, MonotonicTimestamp(3)).is_err());
        session
            .media_button(MediaButton::NavigationGuidance, true, MonotonicTimestamp(3))
            .expect("guidance down");
        assert!(
            session
                .media_button(MediaButton::Play, true, MonotonicTimestamp(4))
                .is_err()
        );
        session
            .siri_button(true, MonotonicTimestamp(4))
            .expect("siri down");
        assert!(session.siri_button(true, MonotonicTimestamp(5)).is_err());
    }

    #[test]
    fn rejected_event_preserves_encoder_and_input_state() {
        let mut session = InputWireSession::new();
        session
            .media_button(MediaButton::Play, true, MonotonicTimestamp(2))
            .expect("press");

        assert!(matches!(
            session.media_button(MediaButton::Play, false, MonotonicTimestamp(1)),
            Err(Error::NonMonotonicTimestamp { .. })
        ));
        assert_eq!(session.event_sequence(), 1);
        assert_eq!(
            session.encoder.last_timestamp(),
            Some(MonotonicTimestamp(2))
        );

        let release = session
            .media_button(MediaButton::Play, false, MonotonicTimestamp(3))
            .expect("release remains valid after rejected event");
        assert_eq!(release.sequence, 2);
    }

    #[test]
    fn failed_release_batch_preserves_all_active_controls() {
        let mut session = InputWireSession::new();
        session
            .media_button(MediaButton::Play, true, MonotonicTimestamp(1))
            .expect("media press");
        session
            .siri_button(true, MonotonicTimestamp(2))
            .expect("Siri press");
        session.encoder.sequence = u32::MAX - 1;

        assert!(matches!(
            session.release_all(MonotonicTimestamp(3)),
            Err(Error::SequenceExhausted)
        ));
        assert_eq!(session.event_sequence(), u32::MAX - 1);
        assert_eq!(
            session.encoder.last_timestamp(),
            Some(MonotonicTimestamp(2))
        );
        assert_eq!(session.media_button, Some(MediaButton::Play));
        assert!(session.siri_down);
    }

    #[test]
    fn teardown_forces_all_releases_once() {
        let mut session = InputWireSession::new();
        session
            .touch(0, TouchAction::Down, 10, 20, MonotonicTimestamp(1))
            .expect("touch");
        session
            .knob_button(KnobButton::Select, true, MonotonicTimestamp(2))
            .expect("knob");
        session
            .media_button(MediaButton::PlayPause, true, MonotonicTimestamp(3))
            .expect("media");
        session
            .telephony_button(TelephonyButton::HookSwitch, true, MonotonicTimestamp(4))
            .expect("phone");
        session
            .siri_button(true, MonotonicTimestamp(5))
            .expect("siri");
        let releases = session.teardown(MonotonicTimestamp(6)).expect("teardown");
        assert_eq!(releases.len(), 5);
        assert_eq!(releases[0].sequence, 6);
        assert_eq!(releases[4].sequence, 10);
        assert!(matches!(
            releases[4].kind,
            AirPlayEventKind::Siri(SiriAction::ButtonUp)
        ));
        assert!(session.is_torn_down());
        assert!(matches!(
            session.rotate(1, MonotonicTimestamp(7)),
            Err(Error::TornDown)
        ));
    }

    #[test]
    fn release_all_clears_active_controls_but_keeps_the_session_live() {
        let mut session = InputWireSession::new();
        session
            .touch(0, TouchAction::Down, 10, 20, MonotonicTimestamp(1))
            .expect("touch");
        session
            .knob_button(KnobButton::Select, true, MonotonicTimestamp(2))
            .expect("knob");
        session
            .siri_button(true, MonotonicTimestamp(3))
            .expect("siri");

        let releases = session
            .release_all(MonotonicTimestamp(4))
            .expect("release active controls");
        assert_eq!(releases.len(), 3);
        assert!(!session.is_torn_down());
        assert!(
            session
                .release_all(MonotonicTimestamp(5))
                .expect("idempotent release")
                .is_empty()
        );
        session
            .rotate(1, MonotonicTimestamp(6))
            .expect("session remains live");
    }

    #[test]
    fn cancel_siri_is_idempotent_and_keeps_the_session_live() {
        let mut session = InputWireSession::new();
        let first = session
            .cancel_siri(MonotonicTimestamp(1))
            .expect("first cancellation");
        assert!(matches!(
            first.kind,
            AirPlayEventKind::Siri(SiriAction::ButtonUp)
        ));
        let second = session
            .cancel_siri(MonotonicTimestamp(2))
            .expect("second cancellation");
        assert!(matches!(
            second.kind,
            AirPlayEventKind::Siri(SiriAction::ButtonUp)
        ));
        assert!(!session.is_torn_down());
    }
}
