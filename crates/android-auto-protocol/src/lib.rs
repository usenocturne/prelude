#![forbid(unsafe_code)]

mod constants;
mod error;
mod frame;
mod proto;
mod session;
mod tls;

pub use constants::{channel, message};
pub use error::{Error, Result};
pub use frame::{FrameDecoder, PhysicalFrame};
pub use proto::{
    DEFAULT_VIDEO_DENSITY_DPI, MAX_HEAD_UNIT_NAME_BYTES, MAX_VIDEO_DENSITY_DPI,
    MIN_VIDEO_DENSITY_DPI, service_discovery_payload, service_discovery_payload_with_density,
};
pub use session::{
    AudioChannel, Button, CancellationToken, Command, DisconnectReason, Event, LINK_SAMPLES,
    LinkWindow, MediaKey, Session, SessionConfig, SessionState, SessionTrace, TouchAction,
    TouchPointer,
};
