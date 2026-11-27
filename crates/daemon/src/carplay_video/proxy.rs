use std::{
    fmt,
    io::{self, Read, Write},
    net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpStream},
    time::Duration,
};

use openssl::ssl::{SslConnector, SslMethod, SslOptions};
use prelude_control::VideoProxyOffer;
use prelude_tls_psk::ExternalPsk;
use zeroize::Zeroizing;

use super::{http, playlist::Remote};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) trait Connection: Read + Write + Send {}

impl<T: Read + Write + Send> Connection for T {}

pub(crate) struct Proxy {
    address: SocketAddrV6,
    server_name: String,
    psk: Zeroizing<Vec<u8>>,
    identity: Vec<u8>,
    authorization: Zeroizing<String>,
}

impl fmt::Debug for Proxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Proxy")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Proxy {
    pub(crate) fn from_offer(offer: &VideoProxyOffer, scope_id: u32) -> Option<Self> {
        let server_name = offer
            .url
            .strip_prefix("https://")
            .unwrap_or(&offer.url)
            .trim_end_matches('/');
        let ip: Ipv6Addr = server_name
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split('%')
            .next()?
            .parse()
            .ok()?;
        if offer.psk.is_empty() || offer.identity.is_empty() || offer.authorization.is_empty() {
            return None;
        }
        Some(Self {
            address: SocketAddrV6::new(ip, offer.port, 0, scope_id),
            server_name: server_name.to_owned(),
            psk: Zeroizing::new(offer.psk.clone()),
            identity: offer.identity.clone(),
            authorization: Zeroizing::new(offer.authorization.clone()),
        })
    }

    pub(crate) fn address(&self) -> SocketAddrV6 {
        self.address
    }

    pub(crate) fn connect(&self, remote: &Remote) -> io::Result<Box<dyn Connection>> {
        let tcp = TcpStream::connect_timeout(&SocketAddr::V6(self.address), CONNECT_TIMEOUT)?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
        tcp.set_nodelay(true)?;
        let mut tunnel = prelude_tls_psk::connect(
            tcp,
            &self.server_name,
            ExternalPsk::new(&self.psk, &self.identity)?,
        )?;
        let target = remote.connect_target();
        let request = format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: {}\r\n\r\n",
            self.authorization.as_str()
        );
        tunnel.write_all(request.as_bytes())?;
        let (head, early) = http::read_head(&mut tunnel)?;
        let status = http::status(&head);
        if status != Some(200) {
            return Err(io::Error::other(format!(
                "the iPhone proxy refused {target}: {}",
                head.lines().next().unwrap_or("no status line")
            )));
        }
        let tunnel = http::Prefixed::new(early, tunnel);
        if !remote.secure {
            return Ok(Box::new(tunnel));
        }
        let mut connector =
            SslConnector::builder(SslMethod::tls_client()).map_err(io::Error::other)?;
        connector.set_options(SslOptions::IGNORE_UNEXPECTED_EOF);
        let connector = connector.build();
        let secured = connector
            .connect(&remote.host, tunnel)
            .map_err(|error| io::Error::other(format!("TLS to {target} failed: {error}")))?;
        Ok(Box::new(secured))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;

    pub(crate) const KEY: [u8; 32] = [7; 32];
    pub(crate) const IDENTITY: &[u8] = b"identity-1";
    pub(crate) const AUTHORIZATION: &str = "Basic dXNlcjpwYXNz";

    pub(crate) struct FakePhone {
        pub(crate) offer: VideoProxyOffer,
        pub(crate) connects: Arc<AtomicUsize>,
    }

    pub(crate) fn fake_phone(
        serve: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static,
    ) -> FakePhone {
        let listener = TcpListener::bind("[::1]:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let connects = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&connects);
        let serve = Arc::new(serve);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let serve = Arc::clone(&serve);
                counted.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let Ok(psk) = ExternalPsk::new(&KEY, IDENTITY) else {
                        return;
                    };
                    let Ok(mut tls) = prelude_tls_psk::accept(stream, psk) else {
                        return;
                    };
                    let Ok((connect, _)) = http::read_head(&mut tls) else {
                        return;
                    };
                    let target = connect
                        .strip_prefix("CONNECT ")
                        .and_then(|rest| rest.split(' ').next())
                        .unwrap_or_default()
                        .to_owned();
                    let authorized = connect
                        .lines()
                        .any(|line| line == format!("Proxy-Authorization: {AUTHORIZATION}"));
                    if !authorized {
                        let _ = tls.write_all(b"HTTP/1.1 407 Denied\r\n\r\n");
                        return;
                    }
                    if tls.write_all(b"HTTP/1.1 200 OK\r\n\r\n").is_err() {
                        return;
                    }
                    let Ok((request, _)) = http::read_head(&mut tls) else {
                        return;
                    };
                    let path = request.split(' ').nth(1).unwrap_or_default().to_owned();
                    let _ = tls.write_all(&serve(&target, &path));
                });
            }
        });
        FakePhone {
            offer: VideoProxyOffer {
                url: "https://[::1]".to_owned(),
                port,
                psk: KEY.to_vec(),
                identity: IDENTITY.to_vec(),
                authorization: AUTHORIZATION.to_owned(),
            },
            connects,
        }
    }

    pub(crate) fn response(content_type: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn offer_parses_the_link_local_proxy() {
        let offer = VideoProxyOffer {
            url: "https://[fe80::1c2b:3aff:fe4d:5e6f]".to_owned(),
            port: 49_200,
            psk: vec![1; 32],
            identity: b"id".to_vec(),
            authorization: "Basic x".to_owned(),
        };
        let proxy = Proxy::from_offer(&offer, 4).expect("proxy");
        assert_eq!(
            proxy.address(),
            SocketAddrV6::new(
                "fe80::1c2b:3aff:fe4d:5e6f".parse().expect("ip"),
                49_200,
                0,
                4
            )
        );
        let mut bad = offer;
        bad.url = "https://phone.local".to_owned();
        assert!(Proxy::from_offer(&bad, 4).is_none());
    }

    #[test]
    fn tunnel_reaches_the_phone_server_through_connect() {
        let phone = fake_phone(|target, path| response("text/plain", &format!("{target}{path}")));
        let proxy = Proxy::from_offer(&phone.offer, 0).expect("proxy");
        let remote = Remote::parse("http://127.0.0.1:58539/master.m3u8").expect("remote");
        let mut connection = proxy.connect(&remote).expect("tunnel");
        connection
            .write_all(b"GET /master.m3u8 HTTP/1.1\r\nHost: 127.0.0.1:58539\r\n\r\n")
            .expect("request");
        let mut answer = String::new();
        connection.read_to_string(&mut answer).expect("answer");
        assert!(answer.ends_with("127.0.0.1:58539/master.m3u8"));
    }

    #[test]
    fn wrong_authorization_is_refused() {
        let phone = fake_phone(|_, _| response("text/plain", "never"));
        let mut offer = phone.offer.clone();
        offer.authorization = "Basic wrong".to_owned();
        let proxy = Proxy::from_offer(&offer, 0).expect("proxy");
        let remote = Remote::parse("http://127.0.0.1:1/x").expect("remote");
        let error = proxy.connect(&remote).err().expect("refused");
        assert!(error.to_string().contains("407"));
    }
}
