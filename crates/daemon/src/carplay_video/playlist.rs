const LOCAL_PREFIX: &str = "/~/";
const MAXIMUM_VARIANT_HEIGHT: u32 = 720;
const BLOCKING_ATTRIBUTES: [&str; 3] =
    ["CAN-BLOCK-RELOAD", "CAN-SKIP-UNTIL", "CAN-SKIP-DATERANGES"];
const FAIRPLAY_KEY_FORMAT: &str = "com.apple.streamingkeydelivery";
const UNSUPPORTED_CODECS: [&str; 4] = ["dvh1", "dvhe", "av01", "vp09"];
const HEVC_CODECS: [&str; 2] = ["hvc1", "hev1"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remote {
    pub(crate) secure: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) target: String,
}

impl Remote {
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else {
            (false, url.strip_prefix("http://")?)
        };
        let (authority, target) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next()?;
        let (host, port) = split_authority(authority, if secure { 443 } else { 80 })?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            secure,
            host,
            port,
            target: target.to_owned(),
        })
    }

    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default = if self.secure { 443 } else { 80 };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    pub(crate) fn connect_target(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{host}:{}", self.port)
    }

    fn scheme(&self) -> &'static str {
        if self.secure { "https" } else { "http" }
    }

    pub(crate) fn url(&self) -> String {
        format!("{}://{}{}", self.scheme(), self.authority(), self.target)
    }

    pub(crate) fn local_path(&self) -> String {
        format!(
            "{LOCAL_PREFIX}{}/{}{}",
            self.scheme(),
            self.authority(),
            self.target
        )
    }
}

fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_owned(), port.parse().ok()?)),
        None => Some((authority.to_owned(), default_port)),
    }
}

pub(crate) fn local_path(url: &str) -> Option<String> {
    Remote::parse(url).map(|remote| remote.local_path())
}

pub(crate) fn remote_for_local(path: &str) -> Option<Remote> {
    let rest = path.strip_prefix(LOCAL_PREFIX)?;
    let (scheme, rest) = rest.split_once('/')?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    Remote::parse(&format!("{scheme}://{rest}"))
}

pub(crate) fn is_playlist(content_type: Option<&str>, body: &[u8]) -> bool {
    content_type.is_some_and(|value| value.to_ascii_lowercase().contains("mpegurl"))
        || body.starts_with(b"#EXTM3U")
}

pub(crate) fn is_fairplay(text: &str) -> bool {
    text.lines()
        .filter(|line| line.starts_with("#EXT-X-KEY:") || line.starts_with("#EXT-X-SESSION-KEY:"))
        .any(|line| {
            attributes(tag_value(line))
                .iter()
                .any(|(name, value)| *name == "KEYFORMAT" && unquote(value) == FAIRPLAY_KEY_FORMAT)
        })
}

const SEGMENT_TAGS: [&str; 6] = [
    "#EXTINF:",
    "#EXT-X-BYTERANGE:",
    "#EXT-X-DISCONTINUITY",
    "#EXT-X-PROGRAM-DATE-TIME:",
    "#EXT-X-GAP",
    "#EXT-X-BITRATE:",
];

pub(crate) fn trim_to(text: &str, start_seconds: f64) -> Option<(String, f64)> {
    if start_seconds <= 0.0 || !text.contains("#EXTINF:") || !text.contains("#EXT-X-ENDLIST") {
        return None;
    }
    let mut header = Vec::new();
    let mut body = Vec::new();
    let mut pending = Vec::new();
    let mut duration = 0.0;
    let mut skipped_seconds = 0.0;
    let mut skipped_segments = 0_u64;
    let mut kept_segments = 0_u64;
    let mut media_sequence = None;
    for line in text.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value.trim().parse::<u64>().ok();
            continue;
        }
        if SEGMENT_TAGS.iter().any(|tag| line.starts_with(tag))
            && !line.starts_with("#EXT-X-DISCONTINUITY-SEQUENCE")
        {
            if let Some(value) = line.strip_prefix("#EXTINF:") {
                duration = value
                    .split(',')
                    .next()
                    .and_then(|seconds| seconds.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
            }
            pending.push(line);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            if kept_segments == 0 {
                header.push(line);
            } else {
                body.push(line);
            }
            continue;
        }
        if kept_segments == 0 && skipped_seconds + duration <= start_seconds {
            skipped_seconds += duration;
            skipped_segments += 1;
        } else {
            kept_segments += 1;
            body.append(&mut pending);
            body.push(line);
        }
        pending.clear();
        duration = 0.0;
    }
    if kept_segments == 0 || skipped_segments == 0 {
        return None;
    }
    let sequence = format!(
        "#EXT-X-MEDIA-SEQUENCE:{}",
        media_sequence.unwrap_or(0) + skipped_segments
    );
    let mut lines = Vec::with_capacity(header.len() + body.len() + 1);
    let mut inserted = false;
    for line in header {
        lines.push(line.to_owned());
        if !inserted && line.starts_with("#EXTM3U") {
            lines.push(sequence.clone());
            inserted = true;
        }
    }
    if !inserted {
        lines.insert(0, sequence);
    }
    lines.extend(body.into_iter().map(str::to_owned));
    lines.push(String::new());
    Some((lines.join("\n"), skipped_seconds))
}

pub(crate) fn rewrite(base: &Remote, text: &str) -> String {
    let filtered = filter_variants(text);
    let mut output = String::with_capacity(filtered.len() + 256);
    for line in filtered.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            output.push('\n');
            continue;
        }
        if line.starts_with("#EXT-X-SERVER-CONTROL:") {
            output.push_str(&without_blocking_reload(line));
        } else if line.starts_with('#') {
            output.push_str(&rewrite_tag_uri(base, line));
        } else {
            output.push_str(&rewrite_uri(base, line));
        }
        output.push('\n');
    }
    output
}

fn rewrite_uri(base: &Remote, uri: &str) -> String {
    let absolute = if uri.starts_with("http://") || uri.starts_with("https://") {
        uri.to_owned()
    } else if let Some(rest) = uri.strip_prefix("//") {
        format!("{}://{rest}", base.scheme())
    } else if uri.starts_with('/') {
        format!("{}://{}{uri}", base.scheme(), base.authority())
    } else {
        return uri.to_owned();
    };
    local_path(&absolute).unwrap_or(absolute)
}

fn rewrite_tag_uri(base: &Remote, line: &str) -> String {
    let Some(start) = line.find("URI=\"") else {
        return line.to_owned();
    };
    let value_start = start + "URI=\"".len();
    let Some(length) = line[value_start..].find('"') else {
        return line.to_owned();
    };
    let value_end = value_start + length;
    format!(
        "{}{}{}",
        &line[..value_start],
        rewrite_uri(base, &line[value_start..value_end]),
        &line[value_end..]
    )
}

fn without_blocking_reload(line: &str) -> String {
    let kept: Vec<String> = attributes(tag_value(line))
        .into_iter()
        .filter(|(name, _)| !BLOCKING_ATTRIBUTES.contains(name))
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    format!("#EXT-X-SERVER-CONTROL:{}", kept.join(","))
}

#[derive(Debug)]
struct Variant<'a> {
    tag: &'a str,
    uri: &'a str,
    decodable: bool,
    hevc: bool,
    height: Option<u32>,
}

fn filter_variants(text: &str) -> String {
    if !text.contains("#EXT-X-STREAM-INF:") {
        return text.to_owned();
    }
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let mut variants = Vec::new();
    let mut header = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("#EXT-X-STREAM-INF:") {
            let uri = lines[index + 1..]
                .iter()
                .position(|candidate| !candidate.is_empty() && !candidate.starts_with('#'))
                .map(|offset| index + 1 + offset);
            if let Some(uri_index) = uri {
                variants.push(variant(line, lines[uri_index]));
                index = uri_index + 1;
                continue;
            }
        } else if !line.starts_with("#EXT-X-I-FRAME-STREAM-INF:") {
            header.push(line);
        }
        index += 1;
    }
    let decodable: Vec<&Variant<'_>> = variants
        .iter()
        .filter(|variant| variant.decodable)
        .collect();
    if decodable.is_empty() {
        return text.to_owned();
    }
    let avc: Vec<&Variant<'_>> = decodable
        .iter()
        .copied()
        .filter(|variant| !variant.hevc)
        .collect();
    let decodable = if avc.is_empty() { decodable } else { avc };
    let small: Vec<&Variant<'_>> = decodable
        .iter()
        .copied()
        .filter(|variant| {
            variant
                .height
                .is_some_and(|height| height <= MAXIMUM_VARIANT_HEIGHT)
        })
        .collect();
    let chosen = if small.is_empty() { decodable } else { small };
    let mut output = String::with_capacity(text.len());
    for line in header {
        output.push_str(line);
        output.push('\n');
    }
    for variant in chosen {
        output.push_str(variant.tag);
        output.push('\n');
        output.push_str(variant.uri);
        output.push('\n');
    }
    output
}

fn variant<'a>(tag: &'a str, uri: &'a str) -> Variant<'a> {
    let attributes = attributes(tag_value(tag));
    let codecs = attributes
        .iter()
        .find(|(name, _)| *name == "CODECS")
        .map(|(_, value)| unquote(value).to_ascii_lowercase());
    let has_codec = |list: &[&str]| {
        codecs.as_deref().is_some_and(|codecs| {
            codecs
                .split(',')
                .any(|codec| list.iter().any(|prefix| codec.trim().starts_with(prefix)))
        })
    };
    let decodable = !has_codec(&UNSUPPORTED_CODECS);
    let hevc = has_codec(&HEVC_CODECS);
    let height = attributes
        .iter()
        .find(|(name, _)| *name == "RESOLUTION")
        .and_then(|(_, value)| value.split_once('x'))
        .and_then(|(_, height)| height.parse().ok());
    Variant {
        tag,
        uri,
        decodable,
        hevc,
        height,
    }
}

fn tag_value(line: &str) -> &str {
    line.split_once(':').map_or("", |(_, value)| value)
}

fn attributes(list: &str) -> Vec<(&str, &str)> {
    let mut attributes = Vec::new();
    let mut rest = list;
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else {
            break;
        };
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => {
                    let value_end = end + 2;
                    (&after[..value_end], &after[value_end..])
                }
                None => (after, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        attributes.push((name.trim(), value));
        rest = next.strip_prefix(',').unwrap_or(next);
    }
    attributes
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_round_trip_through_local_paths() {
        for url in [
            "http://127.0.0.1:58539/master.m3u8?token=a",
            "https://cdn.example.com/v/1/index.m3u8",
            "http://[fe80::1]:8080/a.ts",
        ] {
            let path = local_path(url).expect("local path");
            assert!(path.starts_with("/~/"));
            assert_eq!(remote_for_local(&path).expect("remote").url(), url);
        }
        let remote = Remote::parse("https://cdn.example.com/x").expect("remote");
        assert_eq!(remote.connect_target(), "cdn.example.com:443");
        assert_eq!(remote_for_local("/elsewhere"), None);
        assert_eq!(Remote::parse("ftp://host/x"), None);
    }

    #[test]
    fn playlist_uris_point_back_through_the_gateway() {
        let base = Remote::parse("http://127.0.0.1:5000/v/index.m3u8").expect("base");
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"/init.mp4\"\n#EXTINF:4,\nseg1.ts\n#EXTINF:4,\nhttps://cdn.example.com/seg2.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"//keys.example.com/k\"\n";
        let rewritten = rewrite(&base, text);
        assert!(rewritten.contains("#EXT-X-MAP:URI=\"/~/http/127.0.0.1:5000/init.mp4\""));
        assert!(rewritten.contains("\nseg1.ts\n"));
        assert!(rewritten.contains("\n/~/https/cdn.example.com/seg2.ts\n"));
        assert!(rewritten.contains("URI=\"/~/http/keys.example.com/k\""));
    }

    #[test]
    fn master_playlist_keeps_decodable_variants_up_to_720p() {
        let base = Remote::parse("http://127.0.0.1:5000/master.m3u8").expect("base");
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=854x480,CODECS=\"avc1.4d401f,mp4a.40.2\"\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.640020,mp4a.40.2\"\nmid.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\"\nhigh.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"hvc1.2.4.L123.B0,mp4a.40.2\"\nhevc.m3u8\n#EXT-X-I-FRAME-STREAM-INF:URI=\"iframes.m3u8\"\n";
        let rewritten = rewrite(&base, text);
        assert!(rewritten.contains("low.m3u8"));
        assert!(rewritten.contains("mid.m3u8"));
        assert!(!rewritten.contains("high.m3u8"));
        assert!(!rewritten.contains("hevc.m3u8"));
        assert!(!rewritten.contains("iframes"));
    }

    #[test]
    fn hevc_variants_play_when_no_h264_is_offered() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"hvc1.2.4.L123.B0,mp4a.40.2\"\nhevc.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1920x1080,CODECS=\"hev1.1.6.L120.90,ac-3\"\nfull.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1920x1080,CODECS=\"dvh1.05.06,ec-3\"\ndolby.m3u8\n";
        let filtered = filter_variants(text);
        assert!(filtered.contains("hevc.m3u8"));
        assert!(!filtered.contains("full.m3u8"));
        assert!(!filtered.contains("dolby.m3u8"));
    }

    #[test]
    fn master_playlist_without_small_variants_keeps_every_decodable_one() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080\nhigh.m3u8\n";
        assert_eq!(filter_variants(text), text);
    }

    #[test]
    fn blocking_reload_attributes_are_dropped() {
        let base = Remote::parse("http://h/x.m3u8").expect("base");
        let rewritten = rewrite(
            &base,
            "#EXTM3U\n#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0,CAN-SKIP-UNTIL=12.0\n",
        );
        assert!(rewritten.contains("#EXT-X-SERVER-CONTROL:PART-HOLD-BACK=1.0\n"));
    }

    #[test]
    fn vod_playlists_trim_to_the_start_position() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\na.ts\n#EXTINF:4.0,\nb.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nc.ts\n#EXT-X-ENDLIST\n";
        let (trimmed, skipped) = trim_to(text, 9.0).expect("trimmed");
        assert_eq!(skipped, 8.0);
        assert!(trimmed.contains("#EXT-X-MEDIA-SEQUENCE:12\n"));
        assert!(trimmed.contains("#EXT-X-MAP:URI=\"init.mp4\"\n"));
        assert!(!trimmed.contains("a.ts") && !trimmed.contains("b.ts"));
        assert!(trimmed.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.0,\nc.ts\n#EXT-X-ENDLIST"));
        assert_eq!(trim_to(text, 3.0), None);
        assert_eq!(trim_to(text, 40.0), None);
        assert_eq!(trim_to(&text.replace("#EXT-X-ENDLIST\n", ""), 9.0), None);
        assert_eq!(
            trim_to("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n", 9.0),
            None
        );
    }

    #[test]
    fn fairplay_keys_are_detected() {
        assert!(is_fairplay(
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n"
        ));
        assert!(!is_fairplay(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n"
        ));
        assert!(is_playlist(None, b"#EXTM3U\n"));
        assert!(is_playlist(Some("application/vnd.apple.mpegurl"), b""));
    }
}
