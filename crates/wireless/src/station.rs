//! Wi-Fi client ("station") mode: joining a home network or phone hotspot
//! through the USB adapter that normally hosts the projection access point.

use std::{collections::HashMap, fmt, path::Path};

use pbkdf2::pbkdf2_hmac;
use sha1::Sha1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Open,
    /// WPA/WPA2 personal, including WPA2/WPA3 transition networks.
    Psk,
    /// WPA3-only (SAE), enterprise (EAP) or WEP: a stored PSK cannot join it.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub ssid: Vec<u8>,
    pub signal_dbm: i32,
    pub frequency_mhz: u32,
    pub security: Security,
}

impl ScanResult {
    pub fn display_name(&self) -> String {
        String::from_utf8_lossy(&self.ssid).into_owned()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub ssid: Vec<u8>,
    /// Derived WPA PSK; `None` joins an open network.
    pub psk: Option<[u8; 32]>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("ssid", &String::from_utf8_lossy(&self.ssid))
            .field("psk", &self.psk.map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinProgress {
    Connecting,
    Connected,
    WrongPassword,
}

/// Parses wpa_supplicant's `SCAN_RESULTS` reply: one entry per named network,
/// at its strongest signal, strongest first.
pub fn parse_scan_results(text: &str) -> Vec<ScanResult> {
    let mut strongest: HashMap<Vec<u8>, ScanResult> = HashMap::new();
    for line in text.lines().skip(1) {
        let mut fields = line.splitn(5, '\t');
        let (Some(_bssid), Some(frequency), Some(signal), Some(flags), Some(ssid)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let ssid = decode_ssid(ssid);
        let (Ok(frequency_mhz), Ok(signal_dbm)) = (frequency.parse(), signal.parse()) else {
            continue;
        };
        if ssid.is_empty() || ssid.iter().all(|byte| *byte == 0) {
            continue;
        }
        let result = ScanResult {
            ssid,
            signal_dbm,
            frequency_mhz,
            security: security_from_flags(flags),
        };
        match strongest.get(&result.ssid) {
            Some(known) if known.signal_dbm >= result.signal_dbm => {}
            _ => {
                strongest.insert(result.ssid.clone(), result);
            }
        }
    }
    let mut results: Vec<ScanResult> = strongest.into_values().collect();
    results.sort_by(|a, b| {
        b.signal_dbm
            .cmp(&a.signal_dbm)
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    results
}

fn security_from_flags(flags: &str) -> Security {
    if flags.contains("EAP") || flags.contains("WEP") {
        Security::Unsupported
    } else if flags.contains("PSK") {
        Security::Psk
    } else if flags.contains("SAE") || flags.contains("WPA") || flags.contains("RSN") {
        Security::Unsupported
    } else {
        Security::Open
    }
}

/// Reverses wpa_supplicant's `printf_encode` escaping.
fn decode_ssid(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 == bytes.len() {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let escaped = bytes[index + 1];
        index += 2;
        match escaped {
            b'x' => {
                let hex = text
                    .get(index..index + 2)
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok());
                if let Some(byte) = hex {
                    decoded.push(byte);
                    index += 2;
                } else {
                    decoded.extend_from_slice(b"\\x");
                }
            }
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'e' => decoded.push(0x1b),
            other => decoded.push(other),
        }
    }
    decoded
}

/// IEEE 802.11i passphrase-to-PSK mapping (PBKDF2-HMAC-SHA1, 4096 rounds).
pub fn derive_psk(ssid: &[u8], passphrase: &str) -> [u8; 32] {
    let mut psk = [0_u8; 32];
    pbkdf2_hmac::<Sha1>(passphrase.as_bytes(), ssid, 4096, &mut psk);
    psk
}

pub fn passphrase_is_valid(passphrase: &str) -> bool {
    (8..=63).contains(&passphrase.len())
        && passphrase.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

/// Base wpa_supplicant configuration; networks are added over the control
/// socket so no credential is written here.
pub fn render_wpa_supplicant(control_dir: &Path, country: &str) -> String {
    format!(
        "ctrl_interface=DIR={}\ncountry={country}\nupdate_config=0\nap_scan=1\n",
        control_dir.display()
    )
}

/// Control-socket commands configuring network `id`. SSID and PSK are sent
/// hex-encoded, so no SSID byte can break the command.
pub fn network_commands(id: &str, credential: &Credential) -> Vec<String> {
    let mut commands = vec![
        format!("SET_NETWORK {id} ssid {}", hex::encode(&credential.ssid)),
        format!("SET_NETWORK {id} scan_ssid 1"),
    ];
    match &credential.psk {
        Some(psk) => commands.push(format!("SET_NETWORK {id} psk {}", hex::encode(psk))),
        None => commands.push(format!("SET_NETWORK {id} key_mgmt NONE")),
    }
    commands.push(format!("ENABLE_NETWORK {id}"));
    commands.push(format!("SELECT_NETWORK {id}"));
    commands
}

/// Reads `STATUS` and `LIST_NETWORKS` replies. wpa_supplicant temporarily
/// disables a network after repeated 4-way handshake failures (a wrong key).
pub fn join_progress(status: &str, list_networks: &str) -> JoinProgress {
    if list_networks
        .lines()
        .skip(1)
        .any(|line| line.contains("[TEMP-DISABLED]"))
    {
        return JoinProgress::WrongPassword;
    }
    if status.lines().any(|line| line == "wpa_state=COMPLETED") {
        JoinProgress::Connected
    } else {
        JoinProgress::Connecting
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const SCAN: &str = "bssid / frequency / signal level / flags / ssid\n\
        aa:aa:aa:aa:aa:01\t2437\t-70\t[WPA2-PSK-CCMP][ESS]\tHome\n\
        aa:aa:aa:aa:aa:02\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tHome\n\
        aa:aa:aa:aa:aa:03\t2412\t-40\t[WPA2-PSK-CCMP][ESS]\t\n\
        aa:aa:aa:aa:aa:04\t2462\t-80\t[WPA2-PSK-CCMP][ESS]\tCafe\\x20Wifi\n\
        aa:aa:aa:aa:aa:05\t2412\t-60\t[ESS]\tOpen Net\n\
        aa:aa:aa:aa:aa:06\t5200\t-55\t[RSN-SAE-CCMP][ESS]\tWPA3 Only\n\
        aa:aa:aa:aa:aa:07\t5220\t-65\t[WPA2-EAP-CCMP][ESS]\tCorp\n\
        aa:aa:aa:aa:aa:08\t5240\t-75\t[WPA2-PSK+SAE-CCMP][ESS]\tTransition\n";

    fn named<'a>(results: &'a [ScanResult], ssid: &[u8]) -> &'a ScanResult {
        results
            .iter()
            .find(|result| result.ssid == ssid)
            .expect("network listed")
    }

    #[test]
    fn psk_matches_ieee_vector() {
        assert_eq!(
            hex::encode(derive_psk(b"IEEE", "password")),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
    }

    #[test]
    fn scan_results_dedupe_hidden_and_escaped() {
        let results = parse_scan_results(SCAN);
        assert_eq!(
            results
                .iter()
                .filter(|result| result.ssid == b"Home")
                .count(),
            1
        );
        let home = named(&results, b"Home");
        assert_eq!(home.signal_dbm, -50);
        assert_eq!(home.frequency_mhz, 5180);
        assert_eq!(home.security, Security::Psk);
        assert!(results.iter().all(|result| !result.ssid.is_empty()));
        assert_eq!(named(&results, b"Cafe Wifi").display_name(), "Cafe Wifi");
        assert_eq!(named(&results, b"Open Net").security, Security::Open);
        assert_eq!(
            named(&results, b"WPA3 Only").security,
            Security::Unsupported
        );
        assert_eq!(named(&results, b"Corp").security, Security::Unsupported);
        assert_eq!(named(&results, b"Transition").security, Security::Psk);
        let signals: Vec<i32> = results.iter().map(|result| result.signal_dbm).collect();
        let mut sorted = signals.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(signals, sorted, "strongest first");
    }

    #[test]
    fn escaped_ssid_bytes_are_decoded() {
        let results = parse_scan_results(
            "bssid / frequency / signal level / flags / ssid\n\
             aa:aa:aa:aa:aa:01\t2437\t-70\t[ESS]\tJoe's \\\"Caf\\xc3\\xa9\\\" \\\\\n",
        );
        assert_eq!(results[0].ssid, "Joe's \"Café\" \\".as_bytes());
    }

    #[test]
    fn ssid_bytes_are_hex_encoded() {
        let ssid = "Joe's \"Café\" \\ ☕";
        let commands = network_commands(
            "0",
            &Credential {
                ssid: ssid.as_bytes().to_vec(),
                psk: Some([0xab; 32]),
            },
        );
        assert!(commands.contains(&format!("SET_NETWORK 0 ssid {}", hex::encode(ssid))));
        assert!(commands.contains(&format!("SET_NETWORK 0 psk {}", "ab".repeat(32))));
        assert!(commands.contains(&"SET_NETWORK 0 scan_ssid 1".to_owned()));
        assert!(commands.contains(&"ENABLE_NETWORK 0".to_owned()));
        assert_eq!(
            commands.last().map(String::as_str),
            Some("SELECT_NETWORK 0")
        );
        assert!(commands.iter().all(|command| !command.contains('"')));
        assert!(!commands.iter().any(|command| command.contains("key_mgmt")));

        let open = network_commands(
            "3",
            &Credential {
                ssid: b"Open Net".to_vec(),
                psk: None,
            },
        );
        assert!(open.contains(&"SET_NETWORK 3 key_mgmt NONE".to_owned()));
        assert!(!open.iter().any(|command| command.contains(" psk ")));
    }

    #[test]
    fn config_has_control_dir_and_country() {
        let config = render_wpa_supplicant(Path::new("/run/prelude/client/wpa"), "CA");
        assert!(config.contains("ctrl_interface=DIR=/run/prelude/client/wpa\n"));
        assert!(config.contains("country=CA\n"));
        assert!(!config.contains("network="));
    }

    #[test]
    fn join_progress_reads_status() {
        let listed = "network id / ssid / bssid / flags\n0\tHome\tany\t[CURRENT]\n";
        assert_eq!(
            join_progress("bssid=aa\nwpa_state=COMPLETED\n", listed),
            JoinProgress::Connected
        );
        assert_eq!(
            join_progress("wpa_state=4WAY_HANDSHAKE\n", listed),
            JoinProgress::Connecting
        );
        assert_eq!(
            join_progress(
                "wpa_state=COMPLETED\n",
                "network id / ssid / bssid / flags\n0\tHome\tany\t[TEMP-DISABLED]\n"
            ),
            JoinProgress::WrongPassword
        );
    }

    #[test]
    fn passphrase_rules() {
        assert!(!passphrase_is_valid("short"));
        assert!(passphrase_is_valid("12345678"));
        assert!(passphrase_is_valid(&"a".repeat(63)));
        assert!(!passphrase_is_valid(&"a".repeat(64)));
        assert!(!passphrase_is_valid("bad\u{1}passphrase"));
        assert!(!passphrase_is_valid("caf\u{e9} passphrase"));
    }

    #[test]
    fn credential_debug_redacts_psk() {
        let credential = Credential {
            ssid: b"Home".to_vec(),
            psk: Some([0xab; 32]),
        };
        let debug = format!("{credential:?}");
        assert!(debug.contains("Home"));
        assert!(!debug.contains("ab, ab") && !debug.contains("171"));
    }
}
