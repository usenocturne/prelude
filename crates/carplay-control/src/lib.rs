#![forbid(unsafe_code)]

mod control_event;
mod error;
mod event;
mod hid;
mod input;
mod session;

pub use control_event::{ControlEvent, ControlEventDecoder, SiriSpeechMode, VideoProxyOffer};
pub use error::{Error, Result};
pub use event::{
    AirPlayEvent, AirPlayEventKind, EventChannelEncoder, InputWireSession, MonotonicTimestamp,
    SiriAction, TouchAction,
};
pub use hid::{
    CARPLAY_HEIGHT_MAX, CARPLAY_WIDTH_MAX, HidDeviceDeclaration, HidDeviceKind, KNOB_HID_UID,
    KnobButton, KnobReport, MEDIA_HID_UID, MediaButton, TELEPHONY_HID_UID, TOUCH_CONTACTS,
    TOUCH_HID_UID, TOUCH_REPORT_LENGTH, TelephonyButton, TouchReport, TouchSample,
    carplay_hid_devices, knob_descriptor, media_descriptor, telephony_descriptor, touch_descriptor,
};
pub use input::{
    Button, DisplayGeometry, EncoderDirection, InputEvent, InputRouter, InputSink, TouchContact,
    TouchPhase,
};
pub use session::{
    ImplementationStatus, ProjectionAuthorization, ProjectionCapabilities, ProjectionEvent,
    ProjectionGate, ProjectionPolicy, ProjectionSession, ProjectionState,
};
