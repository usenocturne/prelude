//! Wi-Fi networks the user joined from Settings, most recently used first.
//! Only the derived WPA PSK is stored, never the passphrase.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use serde::{Deserialize, Serialize};
use tracing::warn;

pub const SAVED_NETWORKS_FILE: &str = "wifi-networks.toml";
pub const MAXIMUM_SAVED_NETWORKS: usize = 8;
const MAXIMUM_FILE_BYTES: u64 = 16 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct SavedNetwork {
    pub ssid: Vec<u8>,
    /// Derived WPA PSK; `None` for an open network.
    pub psk: Option<[u8; 32]>,
}

impl fmt::Debug for SavedNetwork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SavedNetwork")
            .field("ssid", &String::from_utf8_lossy(&self.ssid))
            .field("psk", &self.psk.map(|_| "<redacted>"))
            .finish()
    }
}

/// On-disk form: SSID and PSK hex-encoded so any SSID byte survives TOML.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredNetworks {
    #[serde(default)]
    network: Vec<StoredNetwork>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredNetwork {
    ssid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    psk: Option<String>,
}

impl StoredNetwork {
    fn decode(&self) -> Option<SavedNetwork> {
        let ssid = hex::decode(&self.ssid)
            .ok()
            .filter(|ssid| !ssid.is_empty())?;
        let psk = match &self.psk {
            Some(psk) => Some(hex::decode(psk).ok()?.try_into().ok()?),
            None => None,
        };
        Some(SavedNetwork { ssid, psk })
    }

    fn encode(network: &SavedNetwork) -> Self {
        Self {
            ssid: hex::encode(&network.ssid),
            psk: network.psk.map(hex::encode),
        }
    }
}

/// Saved networks, most recently used first. A missing, unreadable,
/// non-private or malformed file yields none.
pub fn load(state_dir: &Path) -> Vec<SavedNetwork> {
    let path = state_dir.join(SAVED_NETWORKS_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warn!(%error, "could not read saved Wi-Fi networks");
            return Vec::new();
        }
    };
    if !metadata.file_type().is_file()
        || metadata.len() > MAXIMUM_FILE_BYTES
        || metadata.permissions().mode() & 0o077 != 0
    {
        warn!("saved Wi-Fi networks are not a private regular file; ignoring them");
        return Vec::new();
    }
    let stored = fs::read_to_string(&path)
        .map_err(|error| error.to_string())
        .and_then(|input| {
            toml::from_str::<StoredNetworks>(&input).map_err(|error| error.to_string())
        });
    match stored {
        Ok(stored) => {
            let decoded: Option<Vec<SavedNetwork>> =
                stored.network.iter().map(StoredNetwork::decode).collect();
            decoded.unwrap_or_else(|| {
                warn!("saved Wi-Fi networks have an invalid entry; ignoring them");
                Vec::new()
            })
        }
        Err(error) => {
            warn!(%error, "saved Wi-Fi networks are malformed; ignoring them");
            Vec::new()
        }
    }
}

/// Moves `network` to the front, replacing any entry with the same SSID.
pub fn remember(state_dir: &Path, network: SavedNetwork) -> io::Result<()> {
    let mut networks = load(state_dir);
    networks.retain(|saved| saved.ssid != network.ssid);
    networks.insert(0, network);
    networks.truncate(MAXIMUM_SAVED_NETWORKS);
    persist(state_dir, &networks)
}

pub fn forget(state_dir: &Path, ssid: &[u8]) -> io::Result<()> {
    let mut networks = load(state_dir);
    networks.retain(|saved| saved.ssid != ssid);
    persist(state_dir, &networks)
}

fn persist(state_dir: &Path, networks: &[SavedNetwork]) -> io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(SAVED_NETWORKS_FILE);
    let temporary = state_dir.join(format!(".{SAVED_NETWORKS_FILE}.tmp.{}", std::process::id()));
    let stored = StoredNetworks {
        network: networks.iter().map(StoredNetwork::encode).collect(),
    };
    let result = (|| {
        let contents = toml::to_string(&stored).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(state_dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn network(ssid: &str, byte: u8) -> SavedNetwork {
        SavedNetwork {
            ssid: ssid.as_bytes().to_vec(),
            psk: Some([byte; 32]),
        }
    }

    #[test]
    fn remember_round_trips_with_private_mode() {
        let state = tempfile::tempdir().expect("state");
        remember(state.path(), network("Home", 1)).expect("remember");
        remember(
            state.path(),
            SavedNetwork {
                ssid: b"Open".to_vec(),
                psk: None,
            },
        )
        .expect("remember open");
        let mode = fs::metadata(state.path().join(SAVED_NETWORKS_FILE))
            .expect("file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let saved = load(state.path());
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].ssid, b"Open");
        assert_eq!(saved[0].psk, None);
        assert_eq!(saved[1], network("Home", 1));
    }

    #[test]
    fn unusual_ssid_round_trips() {
        let state = tempfile::tempdir().expect("state");
        let ssid = "Joe's \"Café\" \\ ☕";
        remember(state.path(), network(ssid, 7)).expect("remember");
        assert_eq!(load(state.path())[0].ssid, ssid.as_bytes());
    }

    #[test]
    fn most_recent_first_and_capped_at_eight() {
        let state = tempfile::tempdir().expect("state");
        for index in 0..10_u8 {
            remember(state.path(), network(&format!("Net{index}"), index)).expect("remember");
        }
        let saved = load(state.path());
        assert_eq!(saved.len(), MAXIMUM_SAVED_NETWORKS);
        assert_eq!(saved[0].ssid, b"Net9");
        assert_eq!(saved[7].ssid, b"Net2");

        remember(state.path(), network("Net5", 55)).expect("remember again");
        let saved = load(state.path());
        assert_eq!(saved[0], network("Net5", 55));
        assert_eq!(
            saved.iter().filter(|saved| saved.ssid == b"Net5").count(),
            1
        );
    }

    #[test]
    fn forget_removes() {
        let state = tempfile::tempdir().expect("state");
        remember(state.path(), network("Home", 1)).expect("remember");
        remember(state.path(), network("Hotspot", 2)).expect("remember");
        forget(state.path(), b"Home").expect("forget");
        let saved = load(state.path());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].ssid, b"Hotspot");
    }

    #[test]
    fn unreadable_file_loads_empty() {
        let state = tempfile::tempdir().expect("state");
        assert!(load(state.path()).is_empty());
        let path = state.path().join(SAVED_NETWORKS_FILE);
        fs::write(&path, "not [valid toml").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(load(state.path()).is_empty());
        fs::write(&path, "[[network]]\nssid = \"zz\"\n").expect("write");
        assert!(load(state.path()).is_empty());
    }

    #[test]
    fn world_readable_file_is_ignored() {
        let state = tempfile::tempdir().expect("state");
        remember(state.path(), network("Home", 1)).expect("remember");
        let path = state.path().join(SAVED_NETWORKS_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(load(state.path()).is_empty());
    }

    #[test]
    fn debug_redacts_psk() {
        let debug = format!("{:?}", network("Home", 0xab));
        assert!(debug.contains("Home"));
        assert!(!debug.contains("171") && !debug.contains("ab, ab"));
    }
}
