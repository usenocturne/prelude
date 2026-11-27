#![forbid(unsafe_code)]

mod auth_setup;
mod control_cipher;
mod crypto;
mod error;
mod mfisap;
mod negotiation;
mod pair_setup;
mod pair_verify;
mod plist_codec;
mod receiver;
mod rtsp;
mod security;
mod srp_server;
mod storage;
mod tlv8;

pub use auth_setup::AuthSetupHandler;
pub use control_cipher::{
    ControlCipher, ControlCipherLimits, ControlCipherState, ControlKeys, EventChannelCipher,
    EventChannelKeys,
};
pub use error::{Error, Result};
pub use mfisap::{
    MFISAP_VERSION, MfiSapProof, MfiSapRequest, MfiSapResponder, MfiSapState, MfiSigner,
    SignerError,
};
pub use negotiation::{
    ALTERNATE_AUDIO_STREAM_TYPE, ALTERNATE_SCREEN_STREAM_TYPE, AudioDescriptor, AudioStreamKind,
    DATA_CHANNEL_STREAM_TYPE, DataChannelDescriptor, HIGH_AUDIO_STREAM_TYPE,
    MAIN_AUDIO_STREAM_TYPE, MAIN_SCREEN_STREAM_TYPE, MessageChannelDescriptor,
    MicrophoneDescriptor, PeerIdentity, PortProvider, ScreenDescriptor, ScreenKind, SessionPorts,
    StreamDescriptor, StreamKey, StreamPorts, TimingDescriptor, VIDEO_PLAYBACK_CLIENT_TYPE,
};
pub use pair_setup::{PairSetup, PairSetupState};
pub use pair_verify::{PairVerify, PairVerifyState};
pub use plist_codec::PlistLimits;
pub use receiver::{
    AirPlayReceiver, AudioFeedbackAnchor, AudioFeedbackStream, ControlTransport, FlushPoint,
    InfoPayload, ParameterPacket, ReceiverConfig, ReceiverEncryptionKeys, ReceiverEvent,
    ReceiverLimits, ReceiverOutput, ReceiverState,
};
pub use rtsp::{
    EventChannelCodec, EventChannelMessage, HeaderMap, RtspCodec, RtspLimits, RtspRequest,
    RtspResponse, encode_response,
};
pub use security::{DataStreamDirection, PairVerification, SecurityContext, SecurityState};
pub use storage::{AccessoryIdentity, PairingStore, PairingStoreLimits};
pub use tlv8::{Tlv8, Tlv8Item, Tlv8Limits, TlvType};
