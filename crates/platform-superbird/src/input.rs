use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const REL_HWHEEL: u16 = 6;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;

const PHYSICAL_WIDTH: i32 = 480;
const PHYSICAL_HEIGHT: i32 = 800;
const MAXIMUM_CARPLAY_CONTACTS: usize = 2;
const MAXIMUM_ROTARY_STEPS: u32 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MonotonicMicros(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    GpioKeys,
    Rotary,
    Touch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceProfile {
    pub kernel_name: String,
    pub keys: BTreeSet<u16>,
    pub relative_axes: BTreeSet<u16>,
    pub absolute_axes: BTreeSet<u16>,
    pub absolute_ranges: BTreeMap<u16, (i32, i32)>,
}

impl InputDeviceProfile {
    pub fn matches_role(&self, role: DeviceRole, exact_kernel_name: &str) -> bool {
        if self.kernel_name != exact_kernel_name {
            return false;
        }
        match role {
            DeviceRole::GpioKeys => GpioKey::all()
                .into_iter()
                .all(|key| self.keys.contains(&key.linux_code())),
            DeviceRole::Rotary => self.relative_axes.contains(&REL_HWHEEL),
            DeviceRole::Touch => {
                [
                    ABS_MT_SLOT,
                    ABS_MT_TRACKING_ID,
                    ABS_MT_POSITION_X,
                    ABS_MT_POSITION_Y,
                ]
                .into_iter()
                .all(|axis| self.absolute_axes.contains(&axis))
                    && self.absolute_ranges.get(&ABS_MT_POSITION_X) == Some(&(0, 479))
                    && self.absolute_ranges.get(&ABS_MT_POSITION_Y) == Some(&(0, 799))
                    && self
                        .absolute_ranges
                        .get(&ABS_MT_SLOT)
                        .is_some_and(|(minimum, maximum)| *minimum == 0 && *maximum >= 1)
            }
        }
    }
}

impl DeviceRole {
    const fn index(self) -> usize {
        match self {
            Self::GpioKeys => 0,
            Self::Rotary => 1,
            Self::Touch => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpioKey {
    Preset1,
    Preset2,
    Preset3,
    Preset4,
    Mute,
    Back,
    Select,
}

impl GpioKey {
    pub const fn linux_code(self) -> u16 {
        match self {
            Self::Preset1 => 2,
            Self::Preset2 => 3,
            Self::Preset3 => 4,
            Self::Preset4 => 5,
            Self::Mute => 50,
            Self::Back => 1,
            Self::Select => 28,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Preset1 => 0,
            Self::Preset2 => 1,
            Self::Preset3 => 2,
            Self::Preset4 => 3,
            Self::Mute => 4,
            Self::Back => 5,
            Self::Select => 6,
        }
    }

    const fn from_linux_code(code: u16) -> Option<Self> {
        match code {
            2 => Some(Self::Preset1),
            3 => Some(Self::Preset2),
            4 => Some(Self::Preset3),
            5 => Some(Self::Preset4),
            50 => Some(Self::Mute),
            1 => Some(Self::Back),
            28 => Some(Self::Select),
            _ => None,
        }
    }

    pub const fn all() -> [Self; 7] {
        [
            Self::Preset1,
            Self::Preset2,
            Self::Preset3,
            Self::Preset4,
            Self::Mute,
            Self::Back,
            Self::Select,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiriLongPressPolicy {
    pub key: GpioKey,
    pub threshold: Duration,
}

impl Default for SiriLongPressPolicy {
    fn default() -> Self {
        Self {
            key: GpioKey::Mute,
            threshold: Duration::from_millis(800),
        }
    }
}

impl SiriLongPressPolicy {
    fn threshold_micros(self) -> Result<u64, InputParserError> {
        validate_long_press_threshold(self.threshold)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputParserConfig {
    pub siri_long_press: Option<SiriLongPressPolicy>,
    pub invert_rotary: bool,
}

impl Default for InputParserConfig {
    fn default() -> Self {
        Self {
            siri_long_press: Some(SiriLongPressPolicy::default()),
            invert_rotary: false,
        }
    }
}

impl InputParserConfig {
    fn validate(self) -> Result<Self, InputParserError> {
        if let Some(policy) = self.siri_long_press {
            policy.threshold_micros()?;
        }
        Ok(self)
    }
}

fn validate_long_press_threshold(threshold: Duration) -> Result<u64, InputParserError> {
    if !(Duration::from_millis(250)..=Duration::from_secs(5)).contains(&threshold) {
        return Err(InputParserError::InvalidConfig(
            "long-press threshold must be within 250ms..=5s",
        ));
    }
    u64::try_from(threshold.as_micros())
        .map_err(|_| InputParserError::InvalidConfig("long-press threshold cannot be represented"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawInputEvent {
    pub role: DeviceRole,
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
    pub timestamp: MonotonicMicros,
}

impl RawInputEvent {
    pub const fn new(
        role: DeviceRole,
        event_type: u16,
        code: u16,
        value: i32,
        timestamp: MonotonicMicros,
    ) -> Self {
        Self {
            role,
            event_type,
            code,
            value,
            timestamp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchInputPhase {
    Down,
    Move,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobInputButton {
    Back,
    Select,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotaryDirection {
    Clockwise,
    CounterClockwise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LocalPreset(u8);

impl LocalPreset {
    pub const ONE: Self = Self(1);
    pub const TWO: Self = Self(2);
    pub const THREE: Self = Self(3);
    pub const FOUR: Self = Self(4);

    pub const fn number(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for LocalPreset {
    type Error = InputParserError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1..=4 => Ok(Self(value)),
            _ => Err(InputParserError::InvalidConfig(
                "local preset number must be within 1..=4",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for LocalPreset {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalInputAction {
    Mute,
    Preset(LocalPreset),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    SynDropped,
    InvalidInput,
    Unplugged,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformInputEventKind {
    Touch {
        contact_id: u8,
        phase: TouchInputPhase,
        x: u16,
        y: u16,
    },
    KnobButton {
        button: KnobInputButton,
        pressed: bool,
    },
    Rotary {
        direction: RotaryDirection,
        steps: u8,
    },
    SiriButton {
        pressed: bool,
    },
    VoiceAssistantCancel,
    SettingsButton,
    Local(LocalInputAction),
    Reset {
        role: DeviceRole,
        reason: ResetReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInputEvent {
    pub timestamp: MonotonicMicros,
    pub kind: PlatformInputEventKind,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InputParserError {
    #[error("invalid input parser configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("input timestamp moved backwards (previous {previous}, received {received})")]
    TimestampMovedBackwards { previous: u64, received: u64 },
}

#[derive(Debug, Clone, Copy, Default)]
struct TouchSlot {
    tracking_id: Option<i32>,
    x: Option<i32>,
    y: Option<i32>,
    reported_down: bool,
    position_dirty: bool,
    pending_down: bool,
    pending_up: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy)]
struct LongPressState {
    key: GpioKey,
    pressed_at: MonotonicMicros,
    emitted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LongPressAction {
    Siri,
}

impl LongPressAction {
    const fn index(self) -> usize {
        match self {
            Self::Siri => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InputParser {
    config: InputParserConfig,
    long_press_threshold_micros: [Option<u64>; 1],
    last_timestamp: Option<MonotonicMicros>,
    dropped: [bool; 3],
    keys_down: [bool; 7],
    long_press: [Option<LongPressState>; 1],
    voice_assistant_cancel_pending: bool,
    touch_slot: usize,
    touch_slots: [TouchSlot; MAXIMUM_CARPLAY_CONTACTS],
}

impl InputParser {
    pub fn new(config: InputParserConfig) -> Result<Self, InputParserError> {
        let config = config.validate()?;
        let long_press_threshold_micros = [config
            .siri_long_press
            .map(SiriLongPressPolicy::threshold_micros)
            .transpose()?];
        Ok(Self {
            config,
            long_press_threshold_micros,
            last_timestamp: None,
            dropped: [false; 3],
            keys_down: [false; 7],
            long_press: [None; 1],
            voice_assistant_cancel_pending: false,
            touch_slot: 0,
            touch_slots: [TouchSlot::default(); MAXIMUM_CARPLAY_CONTACTS],
        })
    }

    pub fn process(
        &mut self,
        event: RawInputEvent,
    ) -> Result<Vec<PlatformInputEvent>, InputParserError> {
        self.check_timestamp(event.timestamp)?;
        if event.event_type == EV_SYN && event.code == SYN_DROPPED {
            self.dropped[event.role.index()] = true;
            return Ok(self.release_role(event.role, ResetReason::SynDropped, event.timestamp));
        }
        if self.dropped[event.role.index()] {
            if event.event_type == EV_SYN && event.code == SYN_REPORT {
                self.dropped[event.role.index()] = false;
            }
            return Ok(Vec::new());
        }
        let events = match event.role {
            DeviceRole::GpioKeys => self.process_key(event),
            DeviceRole::Rotary => self.process_rotary(event),
            DeviceRole::Touch => self.process_touch(event),
        };
        Ok(events)
    }

    pub fn advance_time(
        &mut self,
        timestamp: MonotonicMicros,
    ) -> Result<Vec<PlatformInputEvent>, InputParserError> {
        self.check_timestamp(timestamp)?;
        let mut events = Vec::new();
        for action in [LongPressAction::Siri] {
            let index = action.index();
            let Some(mut state) = self.long_press[index] else {
                continue;
            };
            if state.emitted {
                continue;
            }
            let Some(threshold_micros) = self.long_press_threshold_micros[index] else {
                return Err(InputParserError::InvalidConfig(
                    "long-press state exists without a configured policy",
                ));
            };
            let deadline = state.pressed_at.0.saturating_add(threshold_micros);
            if timestamp.0 < deadline {
                continue;
            }
            state.emitted = true;
            self.long_press[index] = Some(state);
            self.voice_assistant_cancel_pending = true;
            events.push(input_event(timestamp, long_press_down(action)));
        }
        Ok(events)
    }

    pub fn cancel_all(
        &mut self,
        reason: ResetReason,
        timestamp: MonotonicMicros,
    ) -> Result<Vec<PlatformInputEvent>, InputParserError> {
        self.check_timestamp(timestamp)?;
        let mut events = Vec::new();
        for role in [DeviceRole::GpioKeys, DeviceRole::Rotary, DeviceRole::Touch] {
            events.extend(self.release_role(role, reason, timestamp));
        }
        Ok(events)
    }

    fn check_timestamp(&mut self, timestamp: MonotonicMicros) -> Result<(), InputParserError> {
        if let Some(previous) = self.last_timestamp {
            if timestamp < previous {
                return Err(InputParserError::TimestampMovedBackwards {
                    previous: previous.0,
                    received: timestamp.0,
                });
            }
        }
        self.last_timestamp = Some(timestamp);
        Ok(())
    }

    fn process_key(&mut self, event: RawInputEvent) -> Vec<PlatformInputEvent> {
        if event.event_type == EV_SYN || event.event_type != EV_KEY {
            return Vec::new();
        }
        let Some(key) = GpioKey::from_linux_code(event.code) else {
            return Vec::new();
        };
        if event.value == 2 {
            return Vec::new();
        }
        if !matches!(event.value, 0 | 1) {
            return self.invalid_role(DeviceRole::GpioKeys, event.timestamp);
        }
        let pressed = event.value == 1;
        if self.keys_down[key.index()] == pressed {
            if !pressed {
                return Vec::new();
            }
            return self.invalid_role(DeviceRole::GpioKeys, event.timestamp);
        }
        self.keys_down[key.index()] = pressed;
        if let Some(action) = self.long_press_action_for_key(key) {
            return self.process_long_press_key(action, key, pressed, event.timestamp);
        }
        self.normal_key_event(key, pressed, event.timestamp)
    }

    fn long_press_action_for_key(&self, key: GpioKey) -> Option<LongPressAction> {
        if self.config.siri_long_press.map(|policy| policy.key) == Some(key) {
            return Some(LongPressAction::Siri);
        }
        None
    }

    fn process_long_press_key(
        &mut self,
        action: LongPressAction,
        key: GpioKey,
        pressed: bool,
        timestamp: MonotonicMicros,
    ) -> Vec<PlatformInputEvent> {
        let index = action.index();
        if pressed {
            self.long_press[index] = Some(LongPressState {
                key,
                pressed_at: timestamp,
                emitted: false,
            });
            return Vec::new();
        }
        let Some(state) = self.long_press[index].take() else {
            return self.invalid_role(DeviceRole::GpioKeys, timestamp);
        };
        if state.key != key {
            return self.invalid_role(DeviceRole::GpioKeys, timestamp);
        }
        let elapsed = timestamp.0.saturating_sub(state.pressed_at.0);
        let long = state.emitted
            || self.long_press_threshold_micros[index]
                .is_some_and(|threshold| elapsed >= threshold);
        if long {
            let mut events = Vec::with_capacity(2);
            if !state.emitted {
                events.push(input_event(timestamp, long_press_down(action)));
            }
            if action == LongPressAction::Siri {
                self.voice_assistant_cancel_pending = true;
                events.push(input_event(
                    timestamp,
                    PlatformInputEventKind::SiriButton { pressed: false },
                ));
            }
            events
        } else if action == LongPressAction::Siri && self.voice_assistant_cancel_pending {
            self.voice_assistant_cancel_pending = false;
            vec![input_event(
                timestamp,
                PlatformInputEventKind::VoiceAssistantCancel,
            )]
        } else {
            self.short_key_action(key, timestamp)
        }
    }

    fn normal_key_event(
        &self,
        key: GpioKey,
        pressed: bool,
        timestamp: MonotonicMicros,
    ) -> Vec<PlatformInputEvent> {
        match key {
            GpioKey::Back => vec![input_event(
                timestamp,
                PlatformInputEventKind::KnobButton {
                    button: KnobInputButton::Back,
                    pressed,
                },
            )],
            GpioKey::Select => vec![input_event(
                timestamp,
                PlatformInputEventKind::KnobButton {
                    button: KnobInputButton::Select,
                    pressed,
                },
            )],
            GpioKey::Mute
            | GpioKey::Preset1
            | GpioKey::Preset2
            | GpioKey::Preset3
            | GpioKey::Preset4
                if !pressed =>
            {
                self.short_key_action(key, timestamp)
            }
            _ => Vec::new(),
        }
    }

    fn short_key_action(
        &self,
        key: GpioKey,
        timestamp: MonotonicMicros,
    ) -> Vec<PlatformInputEvent> {
        let kind = match key {
            GpioKey::Mute => PlatformInputEventKind::SettingsButton,
            GpioKey::Preset1 => {
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::ONE))
            }
            GpioKey::Preset2 => {
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::TWO))
            }
            GpioKey::Preset3 => {
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::THREE))
            }
            GpioKey::Preset4 => {
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::FOUR))
            }
            GpioKey::Back => {
                return vec![
                    input_event(
                        timestamp,
                        PlatformInputEventKind::KnobButton {
                            button: KnobInputButton::Back,
                            pressed: true,
                        },
                    ),
                    input_event(
                        timestamp,
                        PlatformInputEventKind::KnobButton {
                            button: KnobInputButton::Back,
                            pressed: false,
                        },
                    ),
                ];
            }
            GpioKey::Select => {
                return vec![
                    input_event(
                        timestamp,
                        PlatformInputEventKind::KnobButton {
                            button: KnobInputButton::Select,
                            pressed: true,
                        },
                    ),
                    input_event(
                        timestamp,
                        PlatformInputEventKind::KnobButton {
                            button: KnobInputButton::Select,
                            pressed: false,
                        },
                    ),
                ];
            }
        };
        vec![input_event(timestamp, kind)]
    }

    fn process_rotary(&mut self, event: RawInputEvent) -> Vec<PlatformInputEvent> {
        if event.event_type == EV_SYN {
            return Vec::new();
        }
        if event.event_type != EV_REL || event.code != REL_HWHEEL || event.value == 0 {
            return self.invalid_role(DeviceRole::Rotary, event.timestamp);
        }
        let positive = event.value > 0;
        let clockwise = positive ^ self.config.invert_rotary;
        let steps = event.value.unsigned_abs().min(MAXIMUM_ROTARY_STEPS) as u8;
        vec![input_event(
            event.timestamp,
            PlatformInputEventKind::Rotary {
                direction: if clockwise {
                    RotaryDirection::Clockwise
                } else {
                    RotaryDirection::CounterClockwise
                },
                steps,
            },
        )]
    }

    fn process_touch(&mut self, event: RawInputEvent) -> Vec<PlatformInputEvent> {
        if event.event_type == EV_SYN {
            if event.code == SYN_REPORT {
                return self.flush_touch_frame(event.timestamp);
            }
            return Vec::new();
        }
        if event.event_type != EV_ABS {
            return Vec::new();
        }
        let valid = match event.code {
            ABS_MT_SLOT => {
                let Ok(slot) = usize::try_from(event.value) else {
                    return self.invalid_role(DeviceRole::Touch, event.timestamp);
                };
                if slot >= MAXIMUM_CARPLAY_CONTACTS {
                    false
                } else {
                    self.touch_slot = slot;
                    true
                }
            }
            ABS_MT_TRACKING_ID => self.update_tracking(event.value),
            ABS_MT_POSITION_X => self.update_touch_x(event.value),
            ABS_MT_POSITION_Y => self.update_touch_y(event.value),
            _ => true,
        };
        if valid {
            Vec::new()
        } else {
            self.invalid_role(DeviceRole::Touch, event.timestamp)
        }
    }

    fn update_tracking(&mut self, tracking_id: i32) -> bool {
        let slot = &mut self.touch_slots[self.touch_slot];
        match tracking_id {
            -1 => {
                let Some(_) = slot.tracking_id else {
                    return false;
                };
                if slot.reported_down {
                    let (Some(x), Some(y)) = (slot.x, slot.y) else {
                        return false;
                    };
                    slot.pending_up = Some((x, y));
                } else {
                    slot.pending_down = false;
                }
                slot.tracking_id = None;
                slot.position_dirty = false;
                true
            }
            0.. => {
                if slot.tracking_id.is_some() {
                    return false;
                }
                slot.tracking_id = Some(tracking_id);
                slot.reported_down = false;
                slot.position_dirty = false;
                slot.pending_down = true;
                true
            }
            _ => false,
        }
    }

    fn update_touch_x(&mut self, x: i32) -> bool {
        let slot = &mut self.touch_slots[self.touch_slot];
        if slot.tracking_id.is_none() || !(0..PHYSICAL_WIDTH).contains(&x) {
            return false;
        }
        slot.x = Some(x);
        slot.position_dirty = true;
        true
    }

    fn update_touch_y(&mut self, y: i32) -> bool {
        let slot = &mut self.touch_slots[self.touch_slot];
        if slot.tracking_id.is_none() || !(0..PHYSICAL_HEIGHT).contains(&y) {
            return false;
        }
        slot.y = Some(y);
        slot.position_dirty = true;
        true
    }

    fn flush_touch_frame(&mut self, timestamp: MonotonicMicros) -> Vec<PlatformInputEvent> {
        let mut events = Vec::new();
        for (contact_id, slot) in self.touch_slots.iter_mut().enumerate() {
            if let Some((x, y)) = slot.pending_up.take() {
                let Some((x, y)) = transform_rotate_270(x, y) else {
                    return self.invalid_role(DeviceRole::Touch, timestamp);
                };
                events.push(touch_event(
                    timestamp,
                    contact_id,
                    TouchInputPhase::Up,
                    x,
                    y,
                ));
                slot.reported_down = false;
            }
            if slot.pending_down {
                let (Some(x), Some(y)) = (slot.x, slot.y) else {
                    return self.invalid_role(DeviceRole::Touch, timestamp);
                };
                let Some((x, y)) = transform_rotate_270(x, y) else {
                    return self.invalid_role(DeviceRole::Touch, timestamp);
                };
                events.push(touch_event(
                    timestamp,
                    contact_id,
                    TouchInputPhase::Down,
                    x,
                    y,
                ));
                slot.reported_down = true;
                slot.pending_down = false;
                slot.position_dirty = false;
            } else if slot.tracking_id.is_some() && slot.position_dirty {
                let (Some(x), Some(y)) = (slot.x, slot.y) else {
                    return self.invalid_role(DeviceRole::Touch, timestamp);
                };
                let Some((x, y)) = transform_rotate_270(x, y) else {
                    return self.invalid_role(DeviceRole::Touch, timestamp);
                };
                events.push(touch_event(
                    timestamp,
                    contact_id,
                    TouchInputPhase::Move,
                    x,
                    y,
                ));
                slot.position_dirty = false;
            }
            if slot.tracking_id.is_none() && slot.pending_up.is_none() {
                slot.reported_down = false;
            }
        }
        events
    }

    fn release_role(
        &mut self,
        role: DeviceRole,
        reason: ResetReason,
        timestamp: MonotonicMicros,
    ) -> Vec<PlatformInputEvent> {
        let mut events = Vec::new();
        match role {
            DeviceRole::GpioKeys => {
                for (key, down) in GpioKey::all().into_iter().zip(self.keys_down) {
                    if !down || self.long_press_action_for_key(key).is_some() {
                        continue;
                    }
                    if let GpioKey::Back | GpioKey::Select = key {
                        events.extend(self.normal_key_event(key, false, timestamp));
                    }
                }
                if self.long_press[LongPressAction::Siri.index()].is_some_and(|state| state.emitted)
                {
                    events.push(input_event(
                        timestamp,
                        PlatformInputEventKind::SiriButton { pressed: false },
                    ));
                }
                self.voice_assistant_cancel_pending = false;
                self.keys_down = [false; 7];
                self.long_press = [None; 1];
            }
            DeviceRole::Rotary => {}
            DeviceRole::Touch => {
                for (contact_id, slot) in self.touch_slots.iter().enumerate() {
                    let release = slot
                        .pending_up
                        .or_else(|| slot.reported_down.then(|| slot.x.zip(slot.y)).flatten());
                    if let Some((x, y)) = release {
                        if let Some((x, y)) = transform_rotate_270(x, y) {
                            events.push(touch_event(
                                timestamp,
                                contact_id,
                                TouchInputPhase::Up,
                                x,
                                y,
                            ));
                        }
                    }
                }
                self.touch_slot = 0;
                self.touch_slots = [TouchSlot::default(); MAXIMUM_CARPLAY_CONTACTS];
            }
        }
        events.push(input_event(
            timestamp,
            PlatformInputEventKind::Reset { role, reason },
        ));
        events
    }

    fn invalid_role(
        &mut self,
        role: DeviceRole,
        timestamp: MonotonicMicros,
    ) -> Vec<PlatformInputEvent> {
        self.dropped[role.index()] = true;
        self.release_role(role, ResetReason::InvalidInput, timestamp)
    }
}

pub fn transform_rotate_270(x: i32, y: i32) -> Option<(u16, u16)> {
    if !(0..PHYSICAL_WIDTH).contains(&x) || !(0..PHYSICAL_HEIGHT).contains(&y) {
        return None;
    }
    Some((y as u16, (PHYSICAL_WIDTH - 1 - x) as u16))
}

fn long_press_down(action: LongPressAction) -> PlatformInputEventKind {
    match action {
        LongPressAction::Siri => PlatformInputEventKind::SiriButton { pressed: true },
    }
}

fn input_event(timestamp: MonotonicMicros, kind: PlatformInputEventKind) -> PlatformInputEvent {
    PlatformInputEvent { timestamp, kind }
}

fn touch_event(
    timestamp: MonotonicMicros,
    contact_id: usize,
    phase: TouchInputPhase,
    x: u16,
    y: u16,
) -> PlatformInputEvent {
    input_event(
        timestamp,
        PlatformInputEventKind::Touch {
            contact_id: contact_id as u8,
            phase,
            x,
            y,
        },
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn raw(role: DeviceRole, event_type: u16, code: u16, value: i32, time: u64) -> RawInputEvent {
        RawInputEvent::new(role, event_type, code, value, MonotonicMicros(time))
    }

    fn touch(parser: &mut InputParser, code: u16, value: i32, time: u64) {
        assert!(
            parser
                .process(raw(DeviceRole::Touch, EV_ABS, code, value, time))
                .expect("touch raw")
                .is_empty()
        );
    }

    fn sync(parser: &mut InputParser, time: u64) -> Vec<PlatformInputEvent> {
        parser
            .process(raw(DeviceRole::Touch, EV_SYN, SYN_REPORT, 0, time))
            .expect("sync")
    }

    #[test]
    fn rotate_270_maps_all_physical_edges() {
        assert_eq!(transform_rotate_270(0, 0), Some((0, 479)));
        assert_eq!(transform_rotate_270(479, 0), Some((0, 0)));
        assert_eq!(transform_rotate_270(0, 799), Some((799, 479)));
        assert_eq!(transform_rotate_270(479, 799), Some((799, 0)));
        assert_eq!(transform_rotate_270(480, 0), None);
        assert_eq!(transform_rotate_270(0, 800), None);
    }

    #[test]
    fn device_profiles_require_exact_name_and_role_capabilities() {
        let profile = InputDeviceProfile {
            kernel_name: "gpio-keys-polled".to_owned(),
            keys: GpioKey::all()
                .into_iter()
                .map(GpioKey::linux_code)
                .collect(),
            relative_axes: BTreeSet::new(),
            absolute_axes: BTreeSet::new(),
            absolute_ranges: BTreeMap::new(),
        };
        assert!(profile.matches_role(DeviceRole::GpioKeys, "gpio-keys-polled"));
        assert!(!profile.matches_role(DeviceRole::GpioKeys, "lookalike"));
        assert!(!profile.matches_role(DeviceRole::Rotary, "gpio-keys-polled"));

        let touch = InputDeviceProfile {
            kernel_name: "tlsc6x".to_owned(),
            keys: BTreeSet::new(),
            relative_axes: BTreeSet::new(),
            absolute_axes: BTreeSet::from([
                ABS_MT_SLOT,
                ABS_MT_TRACKING_ID,
                ABS_MT_POSITION_X,
                ABS_MT_POSITION_Y,
            ]),
            absolute_ranges: BTreeMap::from([
                (ABS_MT_SLOT, (0, 4)),
                (ABS_MT_POSITION_X, (0, 479)),
                (ABS_MT_POSITION_Y, (0, 799)),
            ]),
        };
        assert!(touch.matches_role(DeviceRole::Touch, "tlsc6x"));
        let mut wrong_range = touch.clone();
        wrong_range
            .absolute_ranges
            .insert(ABS_MT_POSITION_X, (0, 480));
        assert!(!wrong_range.matches_role(DeviceRole::Touch, "tlsc6x"));
    }

    #[test]
    fn touch_slots_emit_ordered_down_move_up_at_syn_report() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        touch(&mut parser, ABS_MT_TRACKING_ID, 41, 1);
        touch(&mut parser, ABS_MT_POSITION_X, 10, 2);
        touch(&mut parser, ABS_MT_POSITION_Y, 20, 3);
        assert_eq!(
            sync(&mut parser, 4),
            vec![touch_event(
                MonotonicMicros(4),
                0,
                TouchInputPhase::Down,
                20,
                469
            )]
        );
        touch(&mut parser, ABS_MT_POSITION_X, 11, 5);
        touch(&mut parser, ABS_MT_POSITION_Y, 21, 6);
        assert_eq!(
            sync(&mut parser, 7)[0].kind,
            PlatformInputEventKind::Touch {
                contact_id: 0,
                phase: TouchInputPhase::Move,
                x: 21,
                y: 468,
            }
        );
        touch(&mut parser, ABS_MT_TRACKING_ID, -1, 8);
        assert_eq!(
            sync(&mut parser, 9)[0].kind,
            PlatformInputEventKind::Touch {
                contact_id: 0,
                phase: TouchInputPhase::Up,
                x: 21,
                y: 468,
            }
        );
    }

    #[test]
    fn a_new_contact_inherits_the_slot_position_the_kernel_left_unchanged() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        touch(&mut parser, ABS_MT_TRACKING_ID, 1, 1);
        touch(&mut parser, ABS_MT_POSITION_X, 10, 2);
        touch(&mut parser, ABS_MT_POSITION_Y, 20, 3);
        sync(&mut parser, 4);
        touch(&mut parser, ABS_MT_TRACKING_ID, -1, 5);
        sync(&mut parser, 6);
        touch(&mut parser, ABS_MT_TRACKING_ID, 2, 7);
        touch(&mut parser, ABS_MT_POSITION_Y, 300, 8);
        assert_eq!(
            sync(&mut parser, 9),
            vec![touch_event(
                MonotonicMicros(9),
                0,
                TouchInputPhase::Down,
                300,
                469
            )]
        );
        touch(&mut parser, ABS_MT_TRACKING_ID, -1, 10);
        sync(&mut parser, 11);
        touch(&mut parser, ABS_MT_TRACKING_ID, 3, 12);
        assert_eq!(
            sync(&mut parser, 13),
            vec![touch_event(
                MonotonicMicros(13),
                0,
                TouchInputPhase::Down,
                300,
                469
            )]
        );
    }

    #[test]
    fn two_slots_preserve_contact_ids_and_frame_order() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        touch(&mut parser, ABS_MT_TRACKING_ID, 1, 1);
        touch(&mut parser, ABS_MT_POSITION_X, 0, 2);
        touch(&mut parser, ABS_MT_POSITION_Y, 0, 3);
        touch(&mut parser, ABS_MT_SLOT, 1, 4);
        touch(&mut parser, ABS_MT_TRACKING_ID, 2, 5);
        touch(&mut parser, ABS_MT_POSITION_X, 479, 6);
        touch(&mut parser, ABS_MT_POSITION_Y, 799, 7);
        let events = sync(&mut parser, 8);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].kind,
            PlatformInputEventKind::Touch { contact_id: 0, .. }
        ));
        assert!(matches!(
            events[1].kind,
            PlatformInputEventKind::Touch { contact_id: 1, .. }
        ));
    }

    #[test]
    fn syn_dropped_releases_and_ignores_until_report() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        touch(&mut parser, ABS_MT_TRACKING_ID, 1, 1);
        touch(&mut parser, ABS_MT_POSITION_X, 10, 2);
        touch(&mut parser, ABS_MT_POSITION_Y, 20, 3);
        sync(&mut parser, 4);
        let events = parser
            .process(raw(DeviceRole::Touch, EV_SYN, SYN_DROPPED, 0, 5))
            .expect("drop");
        assert!(matches!(
            events[0].kind,
            PlatformInputEventKind::Touch {
                phase: TouchInputPhase::Up,
                ..
            }
        ));
        assert!(matches!(
            events[1].kind,
            PlatformInputEventKind::Reset {
                reason: ResetReason::SynDropped,
                ..
            }
        ));
        assert!(
            parser
                .process(raw(DeviceRole::Touch, EV_ABS, ABS_MT_POSITION_X, 30, 6))
                .expect("ignored")
                .is_empty()
        );
        assert!(sync(&mut parser, 7).is_empty());
    }

    #[test]
    fn drop_never_releases_a_contact_whose_down_was_not_emitted() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        touch(&mut parser, ABS_MT_TRACKING_ID, 1, 1);
        touch(&mut parser, ABS_MT_POSITION_X, 10, 2);
        touch(&mut parser, ABS_MT_POSITION_Y, 20, 3);
        let events = parser
            .process(raw(DeviceRole::Touch, EV_SYN, SYN_DROPPED, 0, 4))
            .expect("drop");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].kind,
            PlatformInputEventKind::Reset { .. }
        ));
    }

    #[test]
    fn drop_between_tracking_release_and_syn_still_releases_reported_contact() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        touch(&mut parser, ABS_MT_TRACKING_ID, 1, 1);
        touch(&mut parser, ABS_MT_POSITION_X, 10, 2);
        touch(&mut parser, ABS_MT_POSITION_Y, 20, 3);
        sync(&mut parser, 4);
        touch(&mut parser, ABS_MT_TRACKING_ID, -1, 5);
        let events = parser
            .process(raw(DeviceRole::Touch, EV_SYN, SYN_DROPPED, 0, 6))
            .expect("drop");
        assert!(matches!(
            events[0].kind,
            PlatformInputEventKind::Touch {
                phase: TouchInputPhase::Up,
                ..
            }
        ));
    }

    #[test]
    fn gpio_mapping_keeps_presets_and_maps_short_mute_to_settings() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        for (index, (key, expected)) in [
            (
                GpioKey::Preset1,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::ONE)),
            ),
            (
                GpioKey::Preset2,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::TWO)),
            ),
            (
                GpioKey::Preset3,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::THREE)),
            ),
            (
                GpioKey::Preset4,
                PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::FOUR)),
            ),
            (GpioKey::Mute, PlatformInputEventKind::SettingsButton),
        ]
        .into_iter()
        .enumerate()
        {
            let pressed_at = (index as u64) * 100 + 10;
            let released_at = pressed_at + 10;
            assert!(
                parser
                    .process(raw(
                        DeviceRole::GpioKeys,
                        EV_KEY,
                        key.linux_code(),
                        1,
                        pressed_at
                    ))
                    .expect("press")
                    .is_empty()
            );
            let event = parser
                .process(raw(
                    DeviceRole::GpioKeys,
                    EV_KEY,
                    key.linux_code(),
                    0,
                    released_at,
                ))
                .expect("release");
            assert_eq!(event[0].kind, expected);
        }
    }

    #[test]
    fn back_select_and_rotary_map_to_carplay_suitable_types() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        let back = parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 1, 1, 1))
            .expect("back");
        assert_eq!(
            back[0].kind,
            PlatformInputEventKind::KnobButton {
                button: KnobInputButton::Back,
                pressed: true,
            }
        );
        let rotary = parser
            .process(raw(DeviceRole::Rotary, EV_REL, REL_HWHEEL, -999, 2))
            .expect("rotary");
        assert_eq!(
            rotary[0].kind,
            PlatformInputEventKind::Rotary {
                direction: RotaryDirection::CounterClockwise,
                steps: 127,
            }
        );
    }

    #[test]
    fn mute_long_press_is_siri_and_never_local_mute() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 1, 0))
            .expect("press");
        let down = parser
            .advance_time(MonotonicMicros(800_000))
            .expect("deadline");
        assert_eq!(
            down[0].kind,
            PlatformInputEventKind::SiriButton { pressed: true }
        );
        let up = parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 0, 900_000))
            .expect("release");
        assert_eq!(
            up[0].kind,
            PlatformInputEventKind::SiriButton { pressed: false }
        );
        assert!(
            !up.iter()
                .any(|event| event.kind == PlatformInputEventKind::Local(LocalInputAction::Mute))
        );
    }

    #[test]
    fn second_short_mute_press_cancels_voice_instead_of_opening_settings() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 1, 0))
            .expect("voice press");
        let down = parser
            .advance_time(MonotonicMicros(800_000))
            .expect("voice deadline");
        assert_eq!(
            down[0].kind,
            PlatformInputEventKind::SiriButton { pressed: true }
        );
        let release = parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 0, 900_000))
            .expect("voice release");
        assert_eq!(
            release[0].kind,
            PlatformInputEventKind::SiriButton { pressed: false }
        );

        assert!(
            parser
                .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 1, 1_000_000))
                .expect("cancel press")
                .is_empty()
        );
        let cancel = parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 0, 1_010_000))
            .expect("cancel release");
        assert_eq!(cancel[0].kind, PlatformInputEventKind::VoiceAssistantCancel);

        parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 1, 1_100_000))
            .expect("settings press");
        let settings = parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 0, 1_110_000))
            .expect("settings release");
        assert_eq!(settings[0].kind, PlatformInputEventKind::SettingsButton);
    }

    #[test]
    fn configurable_long_press_preserves_the_selected_keys_short_action() {
        let config = InputParserConfig {
            siri_long_press: Some(SiriLongPressPolicy {
                key: GpioKey::Preset1,
                threshold: Duration::from_millis(250),
            }),
            ..InputParserConfig::default()
        };
        let mut short = InputParser::new(config).expect("short parser");
        short
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 2, 1, 0))
            .expect("press");
        let event = short
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 2, 0, 100_000))
            .expect("release");
        assert_eq!(
            event[0].kind,
            PlatformInputEventKind::Local(LocalInputAction::Preset(LocalPreset::ONE))
        );

        let mut long = InputParser::new(config).expect("long parser");
        long.process(raw(DeviceRole::GpioKeys, EV_KEY, 2, 1, 0))
            .expect("press");
        let events = long
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 2, 0, 300_000))
            .expect("release");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].kind,
            PlatformInputEventKind::SiriButton { pressed: true }
        );
        assert_eq!(
            events[1].kind,
            PlatformInputEventKind::SiriButton { pressed: false }
        );
    }

    #[test]
    fn cancellation_releases_an_emitted_siri_hold() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 50, 1, 0))
            .expect("press");
        parser
            .advance_time(MonotonicMicros(800_000))
            .expect("siri down");
        let events = parser
            .cancel_all(ResetReason::Cancelled, MonotonicMicros(900_000))
            .expect("cancel");
        assert!(
            events.iter().any(|event| {
                event.kind == PlatformInputEventKind::SiriButton { pressed: false }
            })
        );
    }

    #[test]
    fn rotary_direction_can_be_inverted_by_platform_policy() {
        let mut parser = InputParser::new(InputParserConfig {
            invert_rotary: true,
            ..InputParserConfig::default()
        })
        .expect("parser");
        let event = parser
            .process(raw(DeviceRole::Rotary, EV_REL, REL_HWHEEL, 1, 1))
            .expect("rotary");
        assert_eq!(
            event[0].kind,
            PlatformInputEventKind::Rotary {
                direction: RotaryDirection::CounterClockwise,
                steps: 1,
            }
        );
    }

    #[test]
    fn cancellation_forces_held_releases() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 1, 1, 1))
            .expect("back down");
        let events = parser
            .cancel_all(ResetReason::Cancelled, MonotonicMicros(2))
            .expect("cancel");
        assert!(events.iter().any(|event| {
            event.kind
                == PlatformInputEventKind::KnobButton {
                    button: KnobInputButton::Back,
                    pressed: false,
                }
        }));
    }

    #[test]
    fn invalid_duplicate_press_fails_closed() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 1, 1, 1))
            .expect("first");
        let events = parser
            .process(raw(DeviceRole::GpioKeys, EV_KEY, 1, 1, 2))
            .expect("duplicate");
        assert!(matches!(
            events.last().map(|event| event.kind),
            Some(PlatformInputEventKind::Reset {
                reason: ResetReason::InvalidInput,
                ..
            })
        ));
    }

    #[test]
    fn backwards_time_is_rejected_without_state_mutation() {
        let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
        parser.advance_time(MonotonicMicros(2)).expect("time");
        assert!(matches!(
            parser.advance_time(MonotonicMicros(1)),
            Err(InputParserError::TimestampMovedBackwards { .. })
        ));
    }

    proptest! {
        #[test]
        fn every_physical_coordinate_maps_inside_landscape(x in 0_i32..480, y in 0_i32..800) {
            let (mapped_x, mapped_y) = transform_rotate_270(x, y).expect("in range");
            prop_assert!(mapped_x < 800);
            prop_assert!(mapped_y < 480);
        }

        #[test]
        fn rotary_steps_are_nonzero_and_bounded(value in any::<i32>().prop_filter("nonzero", |v| *v != 0)) {
            let mut parser = InputParser::new(InputParserConfig::default()).expect("parser");
            let events = parser.process(raw(DeviceRole::Rotary, EV_REL, REL_HWHEEL, value, 1)).expect("rotary");
            let PlatformInputEventKind::Rotary { steps, .. } = events[0].kind else {
                prop_assert!(false, "rotary event");
                return Ok(());
            };
            prop_assert!((1..=127).contains(&steps));
        }
    }
}
