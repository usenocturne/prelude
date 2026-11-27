use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use super::{
    http,
    playlist::{self, Remote},
    proxy::Proxy,
};

const ACCEPT_POLL: Duration = Duration::from_millis(50);
const PLAYER_TIMEOUT: Duration = Duration::from_secs(15);
const MAXIMUM_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;
const RELAY_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatewayNotice {
    FairPlayProtected,
}

#[derive(Debug)]
pub(crate) struct Gateway {
    address: SocketAddr,
    offset: Arc<Mutex<f64>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct Start {
    seconds: f64,
    offset: Arc<Mutex<f64>>,
}

impl Gateway {
    pub(crate) fn open(
        proxy: Arc<Proxy>,
        notices: SyncSender<GatewayNotice>,
        start_seconds: f64,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let offset = Arc::new(Mutex::new(0.0));
        let start = Start {
            seconds: start_seconds,
            offset: Arc::clone(&offset),
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::Builder::new()
            .name("prelude-video-gateway".to_owned())
            .spawn(move || accept_loop(&listener, &proxy, &notices, &start, &worker_stopped))?;
        Ok(Self {
            address,
            offset,
            stopped,
            worker: Some(worker),
        })
    }

    pub(crate) fn offset_seconds(&self) -> f64 {
        self.offset.lock().map_or(0.0, |offset| *offset)
    }

    pub(crate) fn local_url(&self, remote_url: &str) -> Option<String> {
        playlist::local_path(remote_url).map(|path| format!("http://{}{path}", self.address))
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn accept_loop(
    listener: &TcpListener,
    proxy: &Arc<Proxy>,
    notices: &SyncSender<GatewayNotice>,
    start: &Start,
    stopped: &AtomicBool,
) {
    while !stopped.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let proxy = Arc::clone(proxy);
                let notices = notices.clone();
                let start = start.clone();
                let spawned = thread::Builder::new()
                    .name("prelude-video-fetch".to_owned())
                    .spawn(move || {
                        if let Err(error) = serve(stream, &proxy, &notices, &start) {
                            tracing::debug!(%error, "CarPlay video fetch ended early");
                        }
                    });
                if let Err(error) = spawned {
                    tracing::warn!(%error, "CarPlay video gateway could not start a fetch");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(error) => {
                tracing::warn!(%error, "CarPlay video gateway stopped accepting");
                return;
            }
        }
    }
}

fn serve(
    mut player: TcpStream,
    proxy: &Proxy,
    notices: &SyncSender<GatewayNotice>,
    start: &Start,
) -> io::Result<()> {
    player.set_nonblocking(false)?;
    player.set_read_timeout(Some(PLAYER_TIMEOUT))?;
    player.set_write_timeout(Some(PLAYER_TIMEOUT))?;
    let (request, _) = http::read_head(&mut player)?;
    let mut line = request.lines().next().unwrap_or_default().split(' ');
    let method = line.next().unwrap_or_default();
    let path = line.next().unwrap_or_default();
    if !matches!(method, "GET" | "HEAD") {
        return reply_status(&mut player, 405, "Method Not Allowed");
    }
    let Some(remote) = playlist::remote_for_local(path) else {
        return reply_status(&mut player, 404, "Not Found");
    };
    let mut upstream = match proxy.connect(&remote) {
        Ok(upstream) => upstream,
        Err(error) => {
            tracing::warn!(%error, url = %remote.url(), "CarPlay video source is unreachable");
            return reply_status(&mut player, 502, "Bad Gateway");
        }
    };
    let range = http::header(&request, "range")
        .map(|range| format!("Range: {range}\r\n"))
        .unwrap_or_default();
    let ask = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: AppleCoreMedia/1.0.0\r\nAccept: */*\r\n{range}Connection: close\r\n\r\n",
        remote.target,
        remote.authority()
    );
    upstream.write_all(ask.as_bytes())?;
    let (head, early) = http::read_head(&mut upstream)?;
    let status = http::status(&head).unwrap_or(502);
    let playlist_like = status == 200
        && method == "GET"
        && (http::header(&head, "content-type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("mpegurl"))
            || remote
                .target
                .split('?')
                .next()
                .is_some_and(|target| target.ends_with(".m3u8")));
    if playlist_like {
        let body = http::read_body(&head, early, &mut upstream, MAXIMUM_PLAYLIST_BYTES)?;
        return serve_playlist(&mut player, &remote, &head, &body, notices, start);
    }
    player.write_all(relayed_head(&head, &remote).as_bytes())?;
    player.write_all(&early)?;
    let mut buffer = vec![0_u8; RELAY_CHUNK_BYTES];
    loop {
        let length = match upstream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        player.write_all(&buffer[..length])?;
    }
}

fn serve_playlist(
    player: &mut TcpStream,
    remote: &Remote,
    head: &str,
    body: &[u8],
    notices: &SyncSender<GatewayNotice>,
    start: &Start,
) -> io::Result<()> {
    if !playlist::is_playlist(http::header(head, "content-type"), body) {
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        player.write_all(answer.as_bytes())?;
        return player.write_all(body);
    }
    let text = String::from_utf8_lossy(body);
    if playlist::is_fairplay(&text) {
        tracing::warn!(url = %remote.url(), "CarPlay video is FairPlay protected and cannot play here");
        let _ = notices.try_send(GatewayNotice::FairPlayProtected);
    }
    let trimmed = playlist::trim_to(&text, start.seconds);
    let text = match trimmed {
        Some((trimmed, skipped)) => {
            if let Ok(mut offset) = start.offset.lock() {
                *offset = offset.max(skipped);
            }
            tracing::info!(skipped, url = %remote.url(), "CarPlay video starts part way in");
            std::borrow::Cow::Owned(trimmed)
        }
        None => text,
    };
    let rewritten = playlist::rewrite(remote, &text);
    let answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        rewritten.len()
    );
    player.write_all(answer.as_bytes())?;
    player.write_all(rewritten.as_bytes())
}

fn relayed_head(head: &str, remote: &Remote) -> String {
    let mut lines = head.lines();
    let mut relayed = format!("{}\r\n", lines.next().unwrap_or("HTTP/1.1 502 Bad Gateway"));
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "connection" | "keep-alive" | "proxy-connection" => {}
            "location" => {
                let value = value.trim();
                let absolute = if value.starts_with('/') && !value.starts_with("//") {
                    format!(
                        "{}://{}{value}",
                        if remote.secure { "https" } else { "http" },
                        remote.authority()
                    )
                } else {
                    value.to_owned()
                };
                let local = playlist::local_path(&absolute).unwrap_or(absolute);
                relayed.push_str(&format!("Location: {local}\r\n"));
            }
            _ => {
                relayed.push_str(line);
                relayed.push_str("\r\n");
            }
        }
    }
    relayed.push_str("Connection: close\r\n\r\n");
    relayed
}

fn reply_status(player: &mut TcpStream, code: u16, reason: &str) -> io::Result<()> {
    player.write_all(
        format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::carplay_video::proxy::tests::{fake_phone, response};

    fn fetch(url: &str) -> String {
        let rest = url.strip_prefix("http://").expect("local url");
        let (authority, path) = rest.split_at(rest.find('/').expect("path"));
        let mut stream = TcpStream::connect(authority).expect("gateway");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
            .expect("request");
        let mut answer = String::new();
        stream.read_to_string(&mut answer).expect("answer");
        answer
    }

    #[test]
    fn playlists_are_rewritten_and_segments_relayed() {
        let phone = fake_phone(|target, path| match (target, path) {
            ("127.0.0.1:58539", "/master.m3u8") => response(
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXTINF:4,\nseg1.ts\n#EXTINF:4,\nhttps://cdn.example.com/seg2.ts\n",
            ),
            ("127.0.0.1:58539", "/seg1.ts") => response("video/mp2t", "SEGMENT-ONE"),
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        });
        let proxy = Arc::new(Proxy::from_offer(&phone.offer, 0).expect("proxy"));
        let (notices, _notices) = mpsc::sync_channel(4);
        let gateway = Gateway::open(proxy, notices, 0.0).expect("gateway");
        let playlist_url = gateway
            .local_url("http://127.0.0.1:58539/master.m3u8")
            .expect("local url");
        let answer = fetch(&playlist_url);
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(answer.contains("Content-Type: application/vnd.apple.mpegurl\r\n"));
        assert!(answer.contains("\nseg1.ts\n"));
        assert!(answer.contains("\n/~/https/cdn.example.com/seg2.ts\n"));
        let segment_url = playlist_url.replace("master.m3u8", "seg1.ts");
        let answer = fetch(&segment_url);
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(answer.contains("Connection: close\r\n"));
        assert!(answer.ends_with("SEGMENT-ONE"));
    }

    #[test]
    fn fairplay_playlists_raise_a_notice() {
        let phone = fake_phone(|_, _| {
            response(
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n",
            )
        });
        let proxy = Arc::new(Proxy::from_offer(&phone.offer, 0).expect("proxy"));
        let (notices, received) = mpsc::sync_channel(4);
        let gateway = Gateway::open(proxy, notices, 0.0).expect("gateway");
        fetch(&gateway.local_url("http://127.0.0.1:1/x.m3u8").expect("url"));
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)),
            Ok(GatewayNotice::FairPlayProtected)
        );
    }

    #[test]
    fn unknown_paths_are_not_forwarded() {
        let phone = fake_phone(|_, _| response("text/plain", "never"));
        let proxy = Arc::new(Proxy::from_offer(&phone.offer, 0).expect("proxy"));
        let (notices, _received) = mpsc::sync_channel(4);
        let gateway = Gateway::open(proxy, notices, 0.0).expect("gateway");
        let answer = fetch(&format!("http://{}/etc/passwd", gateway.address));
        assert!(answer.starts_with("HTTP/1.1 404"));
        assert_eq!(phone.connects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
