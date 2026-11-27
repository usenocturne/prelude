use std::time::Instant;

use plist::{Dictionary, Value};

const UNKNOWN_DURATION_SECONDS: f64 = 4.0 * 3600.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct PlayerStatus {
    pub(crate) ready: bool,
    pub(crate) playing: bool,
    pub(crate) ended: bool,
    pub(crate) failed: bool,
    pub(crate) position: Option<f64>,
    pub(crate) duration: Option<f64>,
    pub(crate) buffered_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QueueAction {
    Load {
        url: String,
        start_seconds: f64,
        muted: bool,
    },
    SetRate(f64),
    Seek(f64),
    SetMuted(bool),
    Stop,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Handled {
    pub(crate) reply: Option<Value>,
    pub(crate) action: Option<QueueAction>,
    pub(crate) started: bool,
}

#[derive(Debug, Default)]
pub(crate) struct PlayQueue {
    item: Option<String>,
    start_seconds: f64,
    rate: f64,
    since: Option<Instant>,
}

impl PlayQueue {
    pub(crate) fn rate(&self) -> f64 {
        self.rate
    }

    pub(crate) fn position(&self) -> f64 {
        let elapsed = self
            .since
            .map_or(0.0, |since| since.elapsed().as_secs_f64());
        (self.start_seconds + elapsed * self.rate).min(UNKNOWN_DURATION_SECONDS)
    }

    pub(crate) fn handle(&mut self, message: &Dictionary, status: &PlayerStatus) -> Handled {
        let mut handled = Handled::default();
        match string(message, "type") {
            Some("insertPlayQueueItem") => {
                let item = message.get("item").and_then(Value::as_dictionary);
                self.item = item
                    .and_then(|item| string(item, "uuid"))
                    .map(str::to_owned);
                self.start_seconds = item
                    .and_then(|item| seconds(item.get("Start-Position")))
                    .unwrap_or(0.0)
                    .max(0.0);
                self.rate = 0.0;
                self.since = None;
                handled.action =
                    item.and_then(|item| string(item, "Content-Location"))
                        .map(|url| QueueAction::Load {
                            url: url.to_owned(),
                            start_seconds: self.start_seconds,
                            muted: item
                                .and_then(|item| item.get("muted"))
                                .and_then(Value::as_boolean)
                                .unwrap_or(false),
                        });
            }
            Some("setRate") => {
                let rate = seconds(message.get("rate")).unwrap_or(0.0).max(0.0);
                self.start_seconds = self.position();
                handled.started = rate > 0.0 && self.rate == 0.0;
                self.rate = rate;
                self.since = Some(Instant::now());
                handled.action = Some(QueueAction::SetRate(rate));
            }
            Some("seek") => {
                if let Some(to) = seconds(message.get("time")) {
                    self.start_seconds = to.max(0.0);
                    self.since = self.since.map(|_| Instant::now());
                    handled.action = Some(QueueAction::Seek(self.start_seconds));
                }
            }
            Some("stop") => {
                *self = Self::default();
                handled.action = Some(QueueAction::Stop);
            }
            Some("setProperty") if string(message, "property") == Some("muted") => {
                handled.action = message
                    .get("value")
                    .and_then(Value::as_boolean)
                    .map(QueueAction::SetMuted);
            }
            _ => {}
        }
        if string(message, "kind") == Some("request") {
            handled.reply = Some(self.reply(message, status));
        }
        handled
    }

    fn reply(&self, message: &Dictionary, status: &PlayerStatus) -> Value {
        let mut reply = Dictionary::new();
        reply.insert("kind".to_owned(), Value::String("response".to_owned()));
        for key in ["type", "controlMessage", "property", "messageID"] {
            if let Some(value) = message.get(key) {
                reply.insert(key.to_owned(), value.clone());
            }
        }
        if string(message, "type") == Some("playbackInfo") {
            reply.insert("value".to_owned(), self.playback_info(status));
        }
        Value::Dictionary(reply)
    }

    fn playback_info(&self, status: &PlayerStatus) -> Value {
        let duration = status
            .duration
            .filter(|duration| *duration > 0.0)
            .unwrap_or(UNKNOWN_DURATION_SECONDS);
        let position = if status.ended {
            duration
        } else {
            status.position.unwrap_or_else(|| self.position())
        };
        let ready = status.ready && !status.failed;
        let running = status.playing && !status.ended && !status.failed;
        let (empty, full) = match status.buffered_percent {
            Some(percent) => (percent == 0, percent >= 100),
            None => (!ready, ready),
        };
        let range = || {
            let mut range = Dictionary::new();
            range.insert("start".to_owned(), Value::Real(0.0));
            range.insert("duration".to_owned(), Value::Real(duration));
            Value::Array(vec![Value::Dictionary(range)])
        };
        let mut info = Dictionary::new();
        info.insert("duration".to_owned(), Value::Real(duration));
        info.insert("position".to_owned(), Value::Real(position));
        info.insert(
            "rate".to_owned(),
            Value::Real(if running { self.rate } else { 0.0 }),
        );
        info.insert("readyToPlay".to_owned(), Value::Boolean(ready));
        info.insert("playbackBufferEmpty".to_owned(), Value::Boolean(empty));
        info.insert("playbackBufferFull".to_owned(), Value::Boolean(full));
        info.insert(
            "playbackLikelyToKeepUp".to_owned(),
            Value::Boolean(ready && !empty),
        );
        info.insert("loadedTimeRanges".to_owned(), range());
        info.insert("seekableTimeRanges".to_owned(), range());
        if let Some(item) = &self.item {
            info.insert("uuid".to_owned(), Value::String(item.clone()));
        }
        Value::Dictionary(info)
    }
}

pub(crate) fn describe(message: &Dictionary) -> String {
    let mut parts = Vec::new();
    for key in ["type", "kind", "property"] {
        if let Some(value) = string(message, key) {
            parts.push(format!("{key}={value}"));
        }
    }
    if let Some(rate) = seconds(message.get("rate")) {
        parts.push(format!("rate={rate}"));
    }
    if let Some(time) = seconds(message.get("time")) {
        parts.push(format!("time={time:.3}"));
    }
    if parts.is_empty() {
        "untyped".to_owned()
    } else {
        parts.join(" ")
    }
}

fn string<'a>(dictionary: &'a Dictionary, key: &str) -> Option<&'a str> {
    dictionary.get(key).and_then(Value::as_string)
}

fn seconds(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let seconds = match value {
        Value::Real(seconds) => *seconds,
        Value::Integer(integer) => integer
            .as_signed()
            .map(|seconds| seconds as f64)
            .or_else(|| integer.as_unsigned().map(|seconds| seconds as f64))?,
        Value::Dictionary(time) => {
            let value = time.get("value").and_then(number)?;
            let timescale = time
                .get("timescale")
                .and_then(number)
                .filter(|scale| *scale > 0.0)?;
            value / timescale
        }
        _ => return None,
    };
    seconds.is_finite().then_some(seconds)
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_real()
        .or_else(|| value.as_signed_integer().map(|number| number as f64))
        .or_else(|| value.as_unsigned_integer().map(|number| number as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(entries: &[(&str, Value)]) -> Dictionary {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    fn item(url: &str, start: Value) -> Value {
        Value::Dictionary(message(&[
            ("Content-Location", Value::String(url.to_owned())),
            ("uuid", Value::String("item-1".to_owned())),
            ("Start-Position", start),
        ]))
    }

    #[test]
    fn insert_loads_the_item_and_answers_the_request() {
        let mut queue = PlayQueue::default();
        let handled = queue.handle(
            &message(&[
                ("type", Value::String("insertPlayQueueItem".to_owned())),
                ("kind", Value::String("request".to_owned())),
                ("messageID", Value::from(7_u64)),
                (
                    "item",
                    item("http://127.0.0.1:5000/master.m3u8", Value::Real(12.5)),
                ),
            ]),
            &PlayerStatus::default(),
        );
        assert_eq!(
            handled.action,
            Some(QueueAction::Load {
                url: "http://127.0.0.1:5000/master.m3u8".to_owned(),
                start_seconds: 12.5,
                muted: false,
            })
        );
        let reply = handled.reply.expect("reply");
        let reply = reply.as_dictionary().expect("dictionary");
        assert_eq!(
            reply.get("kind"),
            Some(&Value::String("response".to_owned()))
        );
        assert_eq!(reply.get("messageID"), Some(&Value::from(7_u64)));
        assert!(!reply.contains_key("value"));
    }

    #[test]
    fn rate_change_reports_the_start_of_playback_once() {
        let mut queue = PlayQueue::default();
        let play = message(&[
            ("type", Value::String("setRate".to_owned())),
            ("rate", Value::Real(1.0)),
        ]);
        let first = queue.handle(&play, &PlayerStatus::default());
        assert!(first.started);
        assert_eq!(first.action, Some(QueueAction::SetRate(1.0)));
        assert!(!queue.handle(&play, &PlayerStatus::default()).started);
        assert_eq!(queue.rate, 1.0);
    }

    #[test]
    fn playback_info_reflects_the_player() {
        let mut queue = PlayQueue::default();
        queue.handle(
            &message(&[
                ("type", Value::String("insertPlayQueueItem".to_owned())),
                ("item", item("http://x/y.m3u8", Value::from(0_u64))),
            ]),
            &PlayerStatus::default(),
        );
        queue.handle(
            &message(&[
                ("type", Value::String("setRate".to_owned())),
                ("rate", Value::Real(1.0)),
            ]),
            &PlayerStatus::default(),
        );
        let status = PlayerStatus {
            ready: true,
            playing: true,
            position: Some(30.0),
            duration: Some(600.0),
            buffered_percent: Some(100),
            ..PlayerStatus::default()
        };
        let reply = queue
            .handle(
                &message(&[
                    ("type", Value::String("playbackInfo".to_owned())),
                    ("kind", Value::String("request".to_owned())),
                    ("messageID", Value::from(3_u64)),
                ]),
                &status,
            )
            .reply
            .expect("reply");
        let info = reply
            .as_dictionary()
            .and_then(|reply| reply.get("value"))
            .and_then(Value::as_dictionary)
            .expect("info");
        assert_eq!(info.get("position"), Some(&Value::Real(30.0)));
        assert_eq!(info.get("duration"), Some(&Value::Real(600.0)));
        assert_eq!(info.get("rate"), Some(&Value::Real(1.0)));
        assert_eq!(info.get("readyToPlay"), Some(&Value::Boolean(true)));
        assert_eq!(info.get("playbackBufferFull"), Some(&Value::Boolean(true)));
        assert_eq!(info.get("uuid"), Some(&Value::String("item-1".to_owned())));
    }

    #[test]
    fn media_times_decode_from_seconds_and_cmtime() {
        let mut cm = Dictionary::new();
        cm.insert("value".to_owned(), Value::from(90_000_u64));
        cm.insert("timescale".to_owned(), Value::from(600_u64));
        assert_eq!(seconds(Some(&Value::Dictionary(cm))), Some(150.0));
        assert_eq!(seconds(Some(&Value::Real(2.5))), Some(2.5));
        assert_eq!(seconds(Some(&Value::String("x".to_owned()))), None);
    }

    #[test]
    fn stop_resets_the_queue() {
        let mut queue = PlayQueue::default();
        queue.handle(
            &message(&[
                ("type", Value::String("setRate".to_owned())),
                ("rate", Value::Real(1.0)),
            ]),
            &PlayerStatus::default(),
        );
        let handled = queue.handle(
            &message(&[("type", Value::String("stop".to_owned()))]),
            &PlayerStatus::default(),
        );
        assert_eq!(handled.action, Some(QueueAction::Stop));
        assert_eq!(queue.rate, 0.0);
    }
}
