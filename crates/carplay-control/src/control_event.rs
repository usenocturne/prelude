use std::io::Cursor;

use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const BPLIST_MAGIC: &[u8; 8] = b"bplist00";
const MAXIMUM_BODY_BYTES: usize = 64 * 1024;
const MAXIMUM_NODES: usize = 1024;
const MAXIMUM_DEPTH: usize = 16;
const MAXIMUM_VALUE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiriSpeechMode {
    None,
    Speaking,
    Recognizing,
}

impl SiriSpeechMode {
    const fn from_wire(value: i64) -> Option<Self> {
        match value {
            -1 => Some(Self::None),
            1 => Some(Self::Speaking),
            2 => Some(Self::Recognizing),
            _ => None,
        }
    }

    pub const fn is_active(self) -> bool {
        matches!(self, Self::Speaking | Self::Recognizing)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct VideoProxyOffer {
    pub url: String,
    pub port: u16,
    pub psk: Vec<u8>,
    pub identity: Vec<u8>,
    pub authorization: String,
}

impl std::fmt::Debug for VideoProxyOffer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoProxyOffer")
            .field("url", &self.url)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    HostUiRequested,
    VideoPlaybackRequested,
    VideoProxyOffered(VideoProxyOffer),
    VideoMovedToAudioOnly,
    DisableBluetooth,
    DuckAudio {
        duration_ms: u32,
        volume_db: f64,
        linear_gain: f64,
    },
    UnduckAudio {
        duration_ms: u32,
    },
    SiriSpeechChanged {
        mode: SiriSpeechMode,
        active: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ControlEventDecoder {
    speech_active: bool,
}

impl ControlEventDecoder {
    pub const fn speech_active(&self) -> bool {
        self.speech_active
    }

    pub fn decode(&mut self, body: &[u8]) -> Result<Option<ControlEvent>> {
        let dictionary = decode_dictionary(body)?;
        let command_type = dictionary
            .get("type")
            .and_then(Value::as_string)
            .ok_or_else(|| Error::MalformedWire("control command has no string type".to_owned()))?;
        match command_type {
            "requestUI" => {
                let params = optional_params(&dictionary)?;
                let url = params.get("url").and_then(Value::as_string);
                Ok(Some(if url == Some("videoplayback:") {
                    ControlEvent::VideoPlaybackRequested
                } else {
                    ControlEvent::HostUiRequested
                }))
            }
            "setProxyParameters" => {
                let params = optional_params(&dictionary)?;
                Ok(decode_proxy_offer(params).map(ControlEvent::VideoProxyOffered))
            }
            "changeVideoPlaybackToAudioOnly" => Ok(Some(ControlEvent::VideoMovedToAudioOnly)),
            "disableBluetooth" => Ok(Some(ControlEvent::DisableBluetooth)),
            "duckAudio" => {
                let params = optional_params(&dictionary)?;
                let duration_ms = optional_duration(params, "durationMs")?;
                let volume_db = optional_number(params, "volume")?.unwrap_or(0.0);
                let linear_gain = (10_f64.powf(volume_db / 20.0)).clamp(0.0, 1.0);
                Ok(Some(ControlEvent::DuckAudio {
                    duration_ms,
                    volume_db,
                    linear_gain,
                }))
            }
            "unduckAudio" => {
                let params = optional_params(&dictionary)?;
                Ok(Some(ControlEvent::UnduckAudio {
                    duration_ms: optional_duration(params, "durationMs")?,
                }))
            }
            "modesChanged" => self.decode_modes_changed(&dictionary),
            _ => Ok(None),
        }
    }

    fn decode_modes_changed(&mut self, dictionary: &Dictionary) -> Result<Option<ControlEvent>> {
        let params = optional_params(dictionary)?;
        let Some(app_states) = params.get("appStates") else {
            return Ok(None);
        };
        let app_states = app_states.as_array().ok_or_else(|| {
            Error::MalformedWire("modesChanged appStates is not an array".to_owned())
        })?;
        let mut exact_mode = None;
        for state in app_states {
            let state = state.as_dictionary().ok_or_else(|| {
                Error::MalformedWire("modesChanged app state is not a dictionary".to_owned())
            })?;
            if optional_i64(state, "appStateID")? != Some(1) {
                continue;
            }
            let Some(wire_mode) = optional_i64(state, "speechMode")? else {
                continue;
            };
            if let Some(mode) = SiriSpeechMode::from_wire(wire_mode) {
                exact_mode = Some(mode);
            }
        }
        let Some(mode) = exact_mode else {
            return Ok(None);
        };
        let active = mode.is_active();
        if active == self.speech_active {
            return Ok(None);
        }
        self.speech_active = active;
        Ok(Some(ControlEvent::SiriSpeechChanged { mode, active }))
    }
}

fn decode_proxy_offer(params: &Dictionary) -> Option<VideoProxyOffer> {
    let port = params
        .get("proxyPort")
        .and_then(Value::as_unsigned_integer)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)?;
    Some(VideoProxyOffer {
        url: params.get("proxyUrl")?.as_string()?.to_owned(),
        port,
        psk: params.get("proxyPsk")?.as_data()?.to_vec(),
        identity: params.get("proxyPskIdentity")?.as_data()?.to_vec(),
        authorization: params.get("proxyAuthorization")?.as_string()?.to_owned(),
    })
}

fn decode_dictionary(body: &[u8]) -> Result<Dictionary> {
    if body.len() > MAXIMUM_BODY_BYTES {
        return Err(Error::MessageTooLarge {
            section: "control plist",
            actual: body.len(),
            maximum: MAXIMUM_BODY_BYTES,
        });
    }
    if !body.starts_with(BPLIST_MAGIC) {
        return Err(Error::MalformedWire(
            "control body is not an Apple binary plist".to_owned(),
        ));
    }
    let value = Value::from_reader(Cursor::new(body))
        .map_err(|error| Error::MalformedWire(format!("invalid control plist: {error}")))?;
    let mut nodes = 0;
    audit_value(&value, 0, &mut nodes)?;
    match value {
        Value::Dictionary(dictionary) => Ok(dictionary),
        _ => Err(Error::MalformedWire(
            "control plist root is not a dictionary".to_owned(),
        )),
    }
}

fn audit_value(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    if depth > MAXIMUM_DEPTH {
        return Err(Error::MalformedWire(
            "control plist nesting exceeds the limit".to_owned(),
        ));
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or_else(|| Error::MalformedWire("control plist node count overflow".to_owned()))?;
    if *nodes > MAXIMUM_NODES {
        return Err(Error::MalformedWire(
            "control plist has too many values".to_owned(),
        ));
    }
    match value {
        Value::Array(values) => {
            for child in values {
                audit_value(child, depth + 1, nodes)?;
            }
        }
        Value::Dictionary(values) => {
            for (key, child) in values {
                if key.len() > MAXIMUM_VALUE_BYTES {
                    return Err(Error::MalformedWire(
                        "control plist key is too long".to_owned(),
                    ));
                }
                audit_value(child, depth + 1, nodes)?;
            }
        }
        Value::String(value) if value.len() > MAXIMUM_VALUE_BYTES => {
            return Err(Error::MalformedWire(
                "control plist string is too long".to_owned(),
            ));
        }
        Value::Data(value) if value.len() > MAXIMUM_VALUE_BYTES => {
            return Err(Error::MalformedWire(
                "control plist data is too long".to_owned(),
            ));
        }
        _ => {}
    }
    Ok(())
}

fn optional_params(dictionary: &Dictionary) -> Result<&Dictionary> {
    match dictionary.get("params") {
        None => Ok(empty_dictionary()),
        Some(Value::Dictionary(params)) => Ok(params),
        Some(_) => Err(Error::MalformedWire(
            "control command params is not a dictionary".to_owned(),
        )),
    }
}

fn empty_dictionary() -> &'static Dictionary {
    static EMPTY: std::sync::OnceLock<Dictionary> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Dictionary::new)
}

fn optional_duration(dictionary: &Dictionary, key: &str) -> Result<u32> {
    let Some(value) = optional_number(dictionary, key)? else {
        return Ok(0);
    };
    if value < 0.0 || value > f64::from(u32::MAX) || value.fract() != 0.0 {
        return Err(Error::MalformedWire(format!(
            "control {key} is not a non-negative integer millisecond duration"
        )));
    }
    Ok(value as u32)
}

fn optional_i64(dictionary: &Dictionary, key: &str) -> Result<Option<i64>> {
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    if let Some(value) = value.as_signed_integer() {
        return Ok(Some(value));
    }
    if let Some(value) = value.as_unsigned_integer() {
        return i64::try_from(value)
            .map(Some)
            .map_err(|_| Error::MalformedWire(format!("control {key} integer is out of range")));
    }
    Err(Error::MalformedWire(format!(
        "control {key} is not an integer"
    )))
}

fn optional_number(dictionary: &Dictionary, key: &str) -> Result<Option<f64>> {
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    let number = value
        .as_real()
        .or_else(|| value.as_signed_integer().map(|value| value as f64))
        .or_else(|| value.as_unsigned_integer().map(|value| value as f64))
        .ok_or_else(|| Error::MalformedWire(format!("control {key} is not numeric")))?;
    if !number.is_finite() {
        return Err(Error::MalformedWire(format!("control {key} is not finite")));
    }
    Ok(Some(number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(dictionary: Dictionary) -> Vec<u8> {
        let mut encoded = Vec::new();
        Value::Dictionary(dictionary)
            .to_writer_binary(&mut encoded)
            .expect("plist");
        encoded
    }

    fn command(command_type: &str, params: Option<Dictionary>) -> Vec<u8> {
        let mut command = Dictionary::new();
        command.insert("type".to_owned(), Value::String(command_type.to_owned()));
        if let Some(params) = params {
            command.insert("params".to_owned(), Value::Dictionary(params));
        }
        encode(command)
    }

    #[test]
    fn video_request_ui_and_proxy_parameters_decode() {
        let mut decoder = ControlEventDecoder::default();
        let mut video = Dictionary::new();
        video.insert("url".to_owned(), Value::String("videoplayback:".to_owned()));
        assert_eq!(
            decoder
                .decode(&command("requestUI", Some(video)))
                .expect("decode"),
            Some(ControlEvent::VideoPlaybackRequested)
        );
        let mut maps = Dictionary::new();
        maps.insert("url".to_owned(), Value::String("maps:".to_owned()));
        assert_eq!(
            decoder
                .decode(&command("requestUI", Some(maps)))
                .expect("decode"),
            Some(ControlEvent::HostUiRequested)
        );

        let mut proxy = Dictionary::new();
        proxy.insert(
            "proxyUrl".to_owned(),
            Value::String("https://[fe80::1]".to_owned()),
        );
        proxy.insert("proxyPort".to_owned(), Value::from(49_152_u64));
        proxy.insert("proxyPsk".to_owned(), Value::Data(vec![7; 32]));
        proxy.insert("proxyPskIdentity".to_owned(), Value::Data(b"id".to_vec()));
        proxy.insert(
            "proxyAuthorization".to_owned(),
            Value::String("Basic dXNlcjpwYXNz".to_owned()),
        );
        assert_eq!(
            decoder
                .decode(&command("setProxyParameters", Some(proxy.clone())))
                .expect("decode"),
            Some(ControlEvent::VideoProxyOffered(VideoProxyOffer {
                url: "https://[fe80::1]".to_owned(),
                port: 49_152,
                psk: vec![7; 32],
                identity: b"id".to_vec(),
                authorization: "Basic dXNlcjpwYXNz".to_owned(),
            }))
        );
        proxy.remove("proxyPsk");
        assert_eq!(
            decoder
                .decode(&command("setProxyParameters", Some(proxy)))
                .expect("decode"),
            None
        );
    }

    #[test]
    fn decodes_exact_livi_duck_and_unduck_semantics() {
        let mut params = Dictionary::new();
        params.insert("durationMs".to_owned(), Value::from(200_u64));
        params.insert("volume".to_owned(), Value::from(-20_i64));
        let mut decoder = ControlEventDecoder::default();
        let event = decoder
            .decode(&command("duckAudio", Some(params)))
            .expect("decode")
            .expect("event");
        let ControlEvent::DuckAudio {
            duration_ms,
            volume_db,
            linear_gain,
        } = event
        else {
            panic!("duck event")
        };
        assert_eq!(duration_ms, 200);
        assert_eq!(volume_db, -20.0);
        assert!((linear_gain - 0.1).abs() < f64::EPSILON);

        let event = decoder
            .decode(&command("unduckAudio", None))
            .expect("decode")
            .expect("event");
        assert_eq!(event, ControlEvent::UnduckAudio { duration_ms: 0 });
    }

    #[test]
    fn tracks_only_exact_siri_speech_modes() {
        let modes = |mode: i64| {
            let mut state = Dictionary::new();
            state.insert("appStateID".to_owned(), Value::from(1_u64));
            state.insert("speechMode".to_owned(), Value::from(mode));
            let mut params = Dictionary::new();
            params.insert(
                "appStates".to_owned(),
                Value::Array(vec![Value::Dictionary(state)]),
            );
            command("modesChanged", Some(params))
        };
        let mut decoder = ControlEventDecoder::default();
        assert_eq!(
            decoder.decode(&modes(2)).expect("recognizing"),
            Some(ControlEvent::SiriSpeechChanged {
                mode: SiriSpeechMode::Recognizing,
                active: true,
            })
        );
        assert_eq!(decoder.decode(&modes(1)).expect("still active"), None);
        assert_eq!(
            decoder.decode(&modes(-1)).expect("none"),
            Some(ControlEvent::SiriSpeechChanged {
                mode: SiriSpeechMode::None,
                active: false,
            })
        );
        assert_eq!(decoder.decode(&modes(99)).expect("unknown"), None);
    }

    #[test]
    fn request_ui_and_unknown_control_are_bounded() {
        let mut decoder = ControlEventDecoder::default();
        assert_eq!(
            decoder
                .decode(&command("requestUI", None))
                .expect("request"),
            Some(ControlEvent::HostUiRequested)
        );
        assert_eq!(
            decoder
                .decode(&command("disableBluetooth", None))
                .expect("handoff"),
            Some(ControlEvent::DisableBluetooth)
        );
        assert_eq!(
            decoder
                .decode(&command("newCommand", None))
                .expect("unknown"),
            None
        );
        assert!(decoder.decode(b"<?xml version='1.0'?>").is_err());
        assert!(decoder.decode(&vec![0; MAXIMUM_BODY_BYTES + 1]).is_err());
    }

    #[test]
    fn malformed_numeric_fields_fail_closed() {
        let mut params = Dictionary::new();
        params.insert("durationMs".to_owned(), Value::from(-1_i64));
        let mut decoder = ControlEventDecoder::default();
        assert!(decoder.decode(&command("duckAudio", Some(params))).is_err());
    }
}
