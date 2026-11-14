//! Request/reply exchange with a hostapd or wpa_supplicant control socket.

use std::{
    fs,
    os::unix::net::UnixDatagram,
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::{Error, Result};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSocket {
    socket: PathBuf,
    run_dir: PathBuf,
    prefix: &'static str,
}

impl ControlSocket {
    /// `socket` is the daemon's control socket; the client end is bound in
    /// `run_dir` as `<prefix>-client-<pid>-<n>` and removed afterwards.
    pub const fn new(socket: PathBuf, run_dir: PathBuf, prefix: &'static str) -> Self {
        Self {
            socket,
            run_dir,
            prefix,
        }
    }

    pub fn request(&self, command: &str) -> Result<String> {
        static NEXT_CLIENT: AtomicU64 = AtomicU64::new(0);
        let client = self.run_dir.join(format!(
            "{}-client-{}-{}",
            self.prefix,
            process::id(),
            NEXT_CLIENT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&client);
        let exchange = || -> std::io::Result<Vec<u8>> {
            let socket = UnixDatagram::bind(&client)?;
            socket.set_read_timeout(Some(CONTROL_TIMEOUT))?;
            socket.connect(&self.socket)?;
            socket.send(command.as_bytes())?;
            let mut reply = vec![0_u8; REPLY_LIMIT];
            let length = socket.recv(&mut reply)?;
            reply.truncate(length);
            Ok(reply)
        };
        let reply = exchange();
        let _ = fs::remove_file(&client);
        let reply = reply
            .map_err(|error| Error::io_context("talking to a wireless control socket", error))?;
        Ok(String::from_utf8_lossy(&reply).into_owned())
    }
}
