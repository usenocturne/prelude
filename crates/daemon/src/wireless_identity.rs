//! Wireless identity that must survive over-the-air updates.
//!
//! The access point's regulatory country and an explicitly configured
//! passphrase are baked into the read-only image configuration at build time.
//! An update built elsewhere (for example an official release) would otherwise
//! replace them, silently changing the regulatory domain or breaking every
//! paired phone. The first boot that enables wireless seeds this file from the
//! image configuration; afterwards the persisted values win.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::WirelessConfig;

pub const IDENTITY_FILE: &str = "wireless-identity.toml";
const MAXIMUM_IDENTITY_BYTES: u64 = 1024;

/// Persisted access point identity. `passphrase: None` means the access point
/// keeps using the serial-derived or generated passphrase.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WirelessIdentity {
    pub country: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

impl std::fmt::Debug for WirelessIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WirelessIdentity")
            .field("country", &self.country)
            .field("passphrase_configured", &self.passphrase.is_some())
            .finish()
    }
}

impl WirelessIdentity {
    fn from_config(config: &WirelessConfig) -> Self {
        Self {
            country: config.country.clone(),
            passphrase: config.passphrase.clone(),
        }
    }

    /// Mirrors the access point's own validation so a bad value can never be
    /// applied and stop the daemon at startup.
    fn is_valid(&self) -> bool {
        let country_ok =
            self.country.len() == 2 && self.country.bytes().all(|byte| byte.is_ascii_uppercase());
        let passphrase_ok = self.passphrase.as_ref().is_none_or(|passphrase| {
            (8..=63).contains(&passphrase.len())
                && passphrase.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        });
        country_ok && passphrase_ok
    }
}

/// Returns the persisted identity, seeding it from `config` on first use.
/// Unreadable or invalid state falls back to `config` without overwriting it.
pub fn reconcile(config: &WirelessConfig, state_dir: &Path) -> WirelessIdentity {
    let path = state_dir.join(IDENTITY_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file()
                || metadata.len() > MAXIMUM_IDENTITY_BYTES
                || metadata.permissions().mode() & 0o077 != 0
            {
                warn!(
                    "persisted wireless identity is not a private regular file; using configuration"
                );
                return WirelessIdentity::from_config(config);
            }
            match fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|input| {
                    toml::from_str::<WirelessIdentity>(&input).map_err(|error| error.to_string())
                }) {
                Ok(identity) if identity.is_valid() => identity,
                Ok(_) => {
                    warn!("persisted wireless identity has an invalid value; using configuration");
                    WirelessIdentity::from_config(config)
                }
                Err(error) => {
                    warn!(%error, "persisted wireless identity is invalid; using configuration");
                    WirelessIdentity::from_config(config)
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let identity = WirelessIdentity::from_config(config);
            if !identity.is_valid() {
                warn!("configured wireless identity is invalid; not persisting it");
                return identity;
            }
            if let Err(error) = persist(&identity, state_dir) {
                warn!(%error, "could not persist wireless identity");
            }
            identity
        }
        Err(error) => {
            warn!(%error, "could not read persisted wireless identity; using configuration");
            WirelessIdentity::from_config(config)
        }
    }
}

/// Applies the persisted identity. The passphrase is always replaced so a
/// persisted `None` keeps the serial-derived passphrase even when a newer
/// image bakes one in.
pub fn apply(identity: &WirelessIdentity, config: &mut WirelessConfig) {
    config.country.clone_from(&identity.country);
    config.passphrase.clone_from(&identity.passphrase);
}

fn persist(identity: &WirelessIdentity, state_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(IDENTITY_FILE);
    let temporary = state_dir.join(format!(".{IDENTITY_FILE}.tmp.{}", std::process::id()));
    let result = (|| {
        let contents = toml::to_string(identity).map_err(std::io::Error::other)?;
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
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    fn config(country: &str, passphrase: Option<&str>) -> WirelessConfig {
        WirelessConfig {
            country: country.to_owned(),
            passphrase: passphrase.map(str::to_owned),
            ..WirelessConfig::default()
        }
    }

    #[test]
    fn first_boot_seeds_from_config() {
        let state = tempfile::tempdir().expect("temporary state directory");
        let identity = reconcile(&config("DE", Some("custom-pass-123")), state.path());

        assert_eq!(identity.country, "DE");
        assert_eq!(identity.passphrase.as_deref(), Some("custom-pass-123"));
        let path = state.path().join(IDENTITY_FILE);
        assert!(path.is_file());
        let mode = fs::metadata(&path)
            .expect("identity metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn state_wins_over_config() {
        let state = tempfile::tempdir().expect("temporary state directory");
        reconcile(&config("DE", None), state.path());

        let mut next = config("US", None);
        let identity = reconcile(&next, state.path());
        assert_eq!(identity.country, "DE");
        apply(&identity, &mut next);
        assert_eq!(next.country, "DE");
    }

    #[test]
    fn serial_default_passphrase_not_captured() {
        let state = tempfile::tempdir().expect("temporary state directory");
        let mut next = config("US", None);
        let identity = reconcile(&next, state.path());

        assert_eq!(identity.passphrase, None);
        apply(&identity, &mut next);
        assert_eq!(next.passphrase, None);
    }

    #[test]
    fn persisted_none_beats_new_baked_passphrase() {
        let state = tempfile::tempdir().expect("temporary state directory");
        reconcile(&config("US", None), state.path());

        let mut next = config("US", Some("new-baked-pass"));
        let identity = reconcile(&next, state.path());
        apply(&identity, &mut next);
        assert_eq!(next.passphrase, None);
    }

    #[test]
    fn corrupt_file_falls_back() {
        let state = tempfile::tempdir().expect("temporary state directory");
        let path = state.path().join(IDENTITY_FILE);
        fs::write(&path, "not = [valid").expect("corrupt identity");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");

        let identity = reconcile(&config("CA", Some("fallback-pass")), state.path());
        assert_eq!(identity.country, "CA");
        assert_eq!(identity.passphrase.as_deref(), Some("fallback-pass"));
        assert_eq!(fs::read_to_string(&path).expect("identity"), "not = [valid");
    }

    fn write_identity(state: &Path, contents: &str) {
        let path = state.join(IDENTITY_FILE);
        fs::write(&path, contents).expect("write identity");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("identity mode");
    }

    #[test]
    fn parseable_invalid_identity_falls_back() {
        for contents in [
            "country = \"ca\"\n",
            "country = \"\"\n",
            "country = \"CAN\"\n",
            "country = \"CA\"\npassphrase = \"short\"\n",
            "country = \"CA\"\npassphrase = \"bad\\u0001passphrase\"\n",
        ] {
            let state = tempfile::tempdir().expect("temporary state directory");
            write_identity(state.path(), contents);

            let identity = reconcile(&config("US", None), state.path());
            assert_eq!(identity.country, "US", "{contents:?}");
            assert_eq!(identity.passphrase, None, "{contents:?}");
            assert_eq!(
                fs::read_to_string(state.path().join(IDENTITY_FILE)).expect("identity kept"),
                contents,
                "invalid state must not be overwritten"
            );
        }
    }

    #[test]
    fn invalid_config_is_not_seeded() {
        let state = tempfile::tempdir().expect("temporary state directory");
        reconcile(&config("us", None), state.path());
        assert!(!state.path().join(IDENTITY_FILE).exists());

        let identity = reconcile(&config("US", None), state.path());
        assert_eq!(identity.country, "US");
        assert!(state.path().join(IDENTITY_FILE).is_file());
    }

    #[test]
    fn debug_redacts_passphrase() {
        let identity = WirelessIdentity {
            country: "US".to_owned(),
            passphrase: Some("secret-passphrase".to_owned()),
        };
        let debug = format!("{identity:?}");
        assert!(!debug.contains("secret-passphrase"));
        assert!(debug.contains("US"));
    }
}
