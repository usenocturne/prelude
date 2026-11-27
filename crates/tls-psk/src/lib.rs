use std::{
    ffi::{c_int, c_uchar},
    fmt,
    io::{self, Read, Write},
    sync::OnceLock,
};

use foreign_types::ForeignType;
use openssl::{
    ex_data::Index,
    ssl::{
        HandshakeError, Ssl, SslContext, SslMethod, SslOptions, SslStream, SslVerifyMode,
        SslVersion,
    },
};
use openssl_sys::{EVP_MD, SSL, SSL_CIPHER, SSL_SESSION};
use zeroize::Zeroizing;

pub const CIPHER_SUITE: &str = "TLS_AES_256_GCM_SHA384";
const CIPHER_SUITE_ID: [u8; 2] = [0x13, 0x02];
const TLS1_3_VERSION: c_int = 0x0304;

type UseSessionCallback = unsafe extern "C" fn(
    *mut SSL,
    *const EVP_MD,
    *mut *const c_uchar,
    *mut usize,
    *mut *mut SSL_SESSION,
) -> c_int;

type FindSessionCallback =
    unsafe extern "C" fn(*mut SSL, *const c_uchar, usize, *mut *mut SSL_SESSION) -> c_int;

unsafe extern "C" {
    fn SSL_set_psk_use_session_callback(ssl: *mut SSL, callback: Option<UseSessionCallback>);
    fn SSL_set_psk_find_session_callback(ssl: *mut SSL, callback: Option<FindSessionCallback>);
    fn SSL_SESSION_new() -> *mut SSL_SESSION;
    fn SSL_SESSION_free(session: *mut SSL_SESSION);
    fn SSL_SESSION_set1_master_key(
        session: *mut SSL_SESSION,
        key: *const c_uchar,
        length: usize,
    ) -> c_int;
    fn SSL_SESSION_set_cipher(session: *mut SSL_SESSION, cipher: *const SSL_CIPHER) -> c_int;
    fn SSL_SESSION_set_protocol_version(session: *mut SSL_SESSION, version: c_int) -> c_int;
    fn SSL_CIPHER_find(ssl: *mut SSL, id: *const c_uchar) -> *const SSL_CIPHER;
    fn SSL_CIPHER_get_handshake_digest(cipher: *const SSL_CIPHER) -> *const EVP_MD;
    fn SSL_get_ex_data(ssl: *const SSL, index: c_int) -> *mut std::ffi::c_void;
}

pub struct ExternalPsk {
    key: Zeroizing<Vec<u8>>,
    identity: Vec<u8>,
}

impl ExternalPsk {
    pub fn new(key: &[u8], identity: &[u8]) -> io::Result<Self> {
        if key.is_empty() || identity.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an external PSK needs a key and an identity",
            ));
        }
        Ok(Self {
            key: Zeroizing::new(key.to_vec()),
            identity: identity.to_vec(),
        })
    }
}

impl fmt::Debug for ExternalPsk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalPsk")
            .field("identity_bytes", &self.identity.len())
            .finish_non_exhaustive()
    }
}

pub fn connect<S: Read + Write>(
    stream: S,
    server_name: &str,
    psk: ExternalPsk,
) -> io::Result<SslStream<S>> {
    let mut context = SslContext::builder(SslMethod::tls_client()).map_err(io::Error::other)?;
    context
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .map_err(io::Error::other)?;
    context
        .set_ciphersuites(CIPHER_SUITE)
        .map_err(io::Error::other)?;
    context.set_verify(SslVerifyMode::NONE);
    context.set_options(SslOptions::IGNORE_UNEXPECTED_EOF);
    let mut ssl = Ssl::new(&context.build()).map_err(io::Error::other)?;
    ssl.set_hostname(server_name).map_err(io::Error::other)?;
    ssl.set_ex_data(psk_index()?, psk);
    let raw = ssl.as_ptr();
    unsafe { SSL_set_psk_use_session_callback(raw, Some(use_session)) };
    ssl.connect(stream).map_err(handshake_error)
}

pub fn accept<S: Read + Write>(stream: S, psk: ExternalPsk) -> io::Result<SslStream<S>> {
    let mut context = SslContext::builder(SslMethod::tls_server()).map_err(io::Error::other)?;
    context
        .set_min_proto_version(Some(SslVersion::TLS1_3))
        .map_err(io::Error::other)?;
    context
        .set_ciphersuites(CIPHER_SUITE)
        .map_err(io::Error::other)?;
    let mut ssl = Ssl::new(&context.build()).map_err(io::Error::other)?;
    ssl.set_ex_data(psk_index()?, psk);
    let raw = ssl.as_ptr();
    unsafe { SSL_set_psk_find_session_callback(raw, Some(find_session)) };
    ssl.accept(stream).map_err(handshake_error)
}

fn handshake_error<S>(error: HandshakeError<S>) -> io::Error {
    match error {
        HandshakeError::SetupFailure(error) => io::Error::other(error),
        HandshakeError::Failure(stream) | HandshakeError::WouldBlock(stream) => {
            io::Error::other(format!("TLS-PSK handshake failed: {}", stream.error()))
        }
    }
}

fn psk_index() -> io::Result<Index<Ssl, ExternalPsk>> {
    static INDEX: OnceLock<Index<Ssl, ExternalPsk>> = OnceLock::new();
    if let Some(index) = INDEX.get() {
        return Ok(*index);
    }
    let index = Ssl::new_ex_index().map_err(io::Error::other)?;
    Ok(*INDEX.get_or_init(|| index))
}

fn stored_psk<'a>(ssl: *mut SSL) -> Option<&'a ExternalPsk> {
    let index = psk_index().ok()?.as_raw();
    let stored = unsafe { SSL_get_ex_data(ssl, index) }.cast::<ExternalPsk>();
    unsafe { stored.as_ref() }
}

fn psk_session(ssl: *mut SSL, psk: &ExternalPsk) -> *mut SSL_SESSION {
    let cipher = unsafe { SSL_CIPHER_find(ssl, CIPHER_SUITE_ID.as_ptr()) };
    if cipher.is_null() {
        return std::ptr::null_mut();
    }
    let session = unsafe { SSL_SESSION_new() };
    if session.is_null() {
        return session;
    }
    let keyed = unsafe { SSL_SESSION_set1_master_key(session, psk.key.as_ptr(), psk.key.len()) };
    let ciphered = unsafe { SSL_SESSION_set_cipher(session, cipher) };
    let versioned = unsafe { SSL_SESSION_set_protocol_version(session, TLS1_3_VERSION) };
    if keyed != 1 || ciphered != 1 || versioned != 1 {
        unsafe { SSL_SESSION_free(session) };
        return std::ptr::null_mut();
    }
    session
}

unsafe extern "C" fn use_session(
    ssl: *mut SSL,
    digest: *const EVP_MD,
    identity: *mut *const c_uchar,
    identity_length: *mut usize,
    session: *mut *mut SSL_SESSION,
) -> c_int {
    let Some(psk) = stored_psk(ssl) else {
        return 0;
    };
    let cipher = unsafe { SSL_CIPHER_find(ssl, CIPHER_SUITE_ID.as_ptr()) };
    if cipher.is_null() {
        return 0;
    }
    if !digest.is_null() && unsafe { SSL_CIPHER_get_handshake_digest(cipher) } != digest {
        unsafe { session.write(std::ptr::null_mut()) };
        return 1;
    }
    let made = psk_session(ssl, psk);
    if made.is_null() {
        return 0;
    }
    unsafe { session.write(made) };
    unsafe { identity.write(psk.identity.as_ptr()) };
    unsafe { identity_length.write(psk.identity.len()) };
    1
}

unsafe extern "C" fn find_session(
    ssl: *mut SSL,
    identity: *const c_uchar,
    identity_length: usize,
    session: *mut *mut SSL_SESSION,
) -> c_int {
    let Some(psk) = stored_psk(ssl) else {
        return 0;
    };
    let offered = unsafe { std::slice::from_raw_parts(identity, identity_length) };
    if offered != psk.identity.as_slice() {
        unsafe { session.write(std::ptr::null_mut()) };
        return 1;
    }
    let made = psk_session(ssl, psk);
    unsafe { session.write(made) };
    c_int::from(!made.is_null())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use super::*;

    fn serve_once(key: &'static [u8], identity: &'static [u8]) -> (u16, thread::JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("address").port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let Ok(mut tls) = accept(stream, ExternalPsk::new(key, identity).expect("psk")) else {
                return false;
            };
            let mut ask = [0_u8; 4];
            if tls.read_exact(&mut ask).is_err() {
                return false;
            }
            tls.write_all(&ask).is_ok() && &ask == b"ping"
        });
        (port, server)
    }

    #[test]
    fn matching_psk_completes_a_tls13_handshake() {
        let (port, server) = serve_once(&[7; 32], b"identity-1");
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let mut tls = connect(
            stream,
            "[fe80::1]",
            ExternalPsk::new(&[7; 32], b"identity-1").expect("psk"),
        )
        .expect("handshake");
        tls.write_all(b"ping").expect("write");
        let mut echo = [0_u8; 4];
        tls.read_exact(&mut echo).expect("read");
        assert_eq!(&echo, b"ping");
        assert!(server.join().expect("server"));
    }

    #[test]
    fn wrong_key_fails_the_handshake() {
        let (port, server) = serve_once(&[7; 32], b"identity-1");
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let result = connect(
            stream,
            "[fe80::1]",
            ExternalPsk::new(&[9; 32], b"identity-1").expect("psk"),
        );
        assert!(result.is_err());
        assert!(!server.join().expect("server"));
    }

    #[test]
    fn empty_credentials_are_rejected() {
        assert!(ExternalPsk::new(&[], b"id").is_err());
        assert!(ExternalPsk::new(&[1], &[]).is_err());
    }
}
