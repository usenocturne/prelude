use std::{
    ffi::OsString,
    fmt, fs,
    io::{BufRead, BufReader},
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use crate::{Error, Result, control::ControlSocket};

const HOSTAPD_START_TIMEOUT: Duration = Duration::from_secs(15);
pub const CHANNELS_2GHZ: [u8; 13] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
pub const CHANNELS_5GHZ: [u8; 9] = [36, 40, 44, 48, 149, 153, 157, 161, 165];
pub const FALLBACK_2GHZ_CHANNEL: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Ghz2_4,
    Ghz5,
}

impl Band {
    pub const fn of_channel(channel: u8) -> Self {
        if channel <= 14 {
            Self::Ghz2_4
        } else {
            Self::Ghz5
        }
    }

    pub const fn channels(self) -> &'static [u8] {
        match self {
            Self::Ghz2_4 => &CHANNELS_2GHZ,
            Self::Ghz5 => &CHANNELS_5GHZ,
        }
    }

    pub const fn default_channel(self) -> u8 {
        match self {
            Self::Ghz2_4 => FALLBACK_2GHZ_CHANNEL,
            Self::Ghz5 => 36,
        }
    }
}

pub fn is_supported_channel(channel: u8) -> bool {
    Band::of_channel(channel).channels().contains(&channel)
}

pub const fn channel_frequency_mhz(channel: u8) -> u32 {
    match Band::of_channel(channel) {
        Band::Ghz2_4 => 2407 + 5 * channel as u32,
        Band::Ghz5 => 5000 + 5 * channel as u32,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChannelWidth {
    Mhz20,
    Mhz40,
    Mhz80,
}

impl ChannelWidth {
    pub const ALL: [Self; 3] = [Self::Mhz20, Self::Mhz40, Self::Mhz80];

    pub const fn mhz(self) -> u16 {
        match self {
            Self::Mhz20 => 20,
            Self::Mhz40 => 40,
            Self::Mhz80 => 80,
        }
    }

    pub const fn from_mhz(mhz: u16) -> Option<Self> {
        match mhz {
            20 => Some(Self::Mhz20),
            40 => Some(Self::Mhz40),
            80 => Some(Self::Mhz80),
            _ => None,
        }
    }

    pub const fn widest_for(channel: u8) -> Self {
        match (Band::of_channel(channel), channel) {
            (_, 165) => Self::Mhz20,
            (Band::Ghz2_4, _) => Self::Mhz40,
            (Band::Ghz5, _) => Self::Mhz80,
        }
    }

    pub fn supported_on(channel: u8) -> &'static [Self] {
        let widest = Self::widest_for(channel);
        &Self::ALL[..=Self::ALL
            .iter()
            .position(|&width| width == widest)
            .unwrap_or(0)]
    }

    const fn narrower(self) -> Option<Self> {
        match self {
            Self::Mhz80 => Some(Self::Mhz40),
            Self::Mhz40 => Some(Self::Mhz20),
            Self::Mhz20 => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccessPointConfig {
    pub interface: String,
    pub ssid: String,
    pub passphrase: String,
    pub country: String,
    pub channel: u8,
    pub width: ChannelWidth,
    pub address: Ipv4Addr,
    pub run_dir: PathBuf,
}

impl fmt::Debug for AccessPointConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessPointConfig")
            .field("interface", &self.interface)
            .field("ssid", &self.ssid)
            .field("passphrase", &"[redacted]")
            .field("country", &self.country)
            .field("channel", &self.channel)
            .field("width", &self.width)
            .field("address", &self.address)
            .field("run_dir", &self.run_dir)
            .finish()
    }
}

impl AccessPointConfig {
    pub fn validate(&self) -> Result<()> {
        if self.interface.is_empty()
            || self.interface.len() > 15
            || !self
                .interface
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::InvalidConfig("wireless interface name"));
        }
        if self.ssid.is_empty() || self.ssid.len() > 32 || self.ssid.contains(['\n', '\r']) {
            return Err(Error::InvalidConfig("SSID must be 1-32 bytes on one line"));
        }
        if !(8..=63).contains(&self.passphrase.len())
            || !self
                .passphrase
                .bytes()
                .all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(Error::InvalidConfig(
                "WPA2 passphrase must be 8-63 printable ASCII characters",
            ));
        }
        if self.country.len() != 2 || !self.country.bytes().all(|byte| byte.is_ascii_uppercase()) {
            return Err(Error::InvalidConfig(
                "country must be an uppercase ISO 3166 code",
            ));
        }
        if !is_supported_channel(self.channel) {
            return Err(Error::InvalidConfig(
                "channel must be 1-13 or a non-DFS 5 GHz channel (36-48, 149-165)",
            ));
        }
        if !self.address.is_private() || self.address.octets()[3] != 1 {
            return Err(Error::InvalidConfig(
                "access point address must be a private x.y.z.1 address",
            ));
        }
        Ok(())
    }

    pub const fn frequency_mhz(&self) -> u32 {
        channel_frequency_mhz(self.channel)
    }

    pub const fn band(&self) -> Band {
        Band::of_channel(self.channel)
    }

    fn hostapd_path(&self) -> PathBuf {
        self.run_dir.join("hostapd.conf")
    }

    fn dnsmasq_path(&self) -> PathBuf {
        self.run_dir.join("dnsmasq.conf")
    }
}

const BEACON_INTERVAL_TU: u32 = 50;

pub fn render_hostapd(config: &AccessPointConfig, width: ChannelWidth) -> String {
    let hw_mode = match config.band() {
        Band::Ghz2_4 => "g",
        Band::Ghz5 => "a",
    };
    let width = width.min(ChannelWidth::widest_for(config.channel));
    let mut out = format!(
        "interface={interface}\n\
         driver=nl80211\n\
         ctrl_interface={run_dir}/hostapd\n\
         ssid={ssid}\n\
         utf8_ssid=1\n\
         country_code={country}\n\
         ieee80211d=1\n\
         ieee80211h=0\n\
         hw_mode={hw_mode}\n\
         channel={channel}\n\
         beacon_int={BEACON_INTERVAL_TU}\n\
         dtim_period=1\n\
         ieee80211n=1\n\
         wmm_enabled=1\n\
         disassoc_low_ack=0\n\
         auth_algs=1\n\
         ignore_broadcast_ssid=0\n\
         wpa=2\n\
         wpa_key_mgmt=WPA-PSK\n\
         rsn_pairwise=CCMP\n\
         wpa_passphrase={passphrase}\n",
        interface = config.interface,
        run_dir = config.run_dir.display(),
        ssid = config.ssid,
        country = config.country,
        channel = config.channel,
        passphrase = config.passphrase,
    );
    let ht40 = if matches!(config.channel, 1..=7 | 36 | 44 | 149 | 157) {
        "[HT40+]"
    } else {
        "[HT40-]"
    };
    match width {
        ChannelWidth::Mhz20 => out.push_str("ht_capab=[SHORT-GI-20]\n"),
        ChannelWidth::Mhz40 => {
            out.push_str(&format!("ht_capab={ht40}[SHORT-GI-20][SHORT-GI-40]\n"));
        }
        ChannelWidth::Mhz80 => {
            let center = if config.channel <= 48 { 42 } else { 155 };
            out.push_str(&format!(
                "ht_capab={ht40}[SHORT-GI-20][SHORT-GI-40]\n\
                 ieee80211ac=1\n\
                 vht_capab=[SHORT-GI-80]\n\
                 vht_oper_chwidth=1\n\
                 vht_oper_centr_freq_seg0_idx={center}\n"
            ));
        }
    }
    out
}

pub fn render_dnsmasq(config: &AccessPointConfig) -> String {
    let [a, b, c, _] = config.address.octets();
    format!(
        "interface={interface}\n\
         bind-interfaces\n\
         except-interface=lo\n\
         port=0\n\
         pid-file=\n\
         leasefile-ro\n\
         dhcp-authoritative\n\
         dhcp-range={a}.{b}.{c}.10,{a}.{b}.{c}.50,255.255.255.0,12h\n\
         dhcp-option=option:router\n\
         dhcp-option=option:dns-server\n",
        interface = config.interface,
    )
}

fn dnsmasq_args(conf: &Path) -> [OsString; 2] {
    let mut conf_file = OsString::from("--conf-file=");
    conf_file.push(conf);
    [OsString::from("--keep-in-foreground"), conf_file]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPointStatus {
    pub bssid: String,
    pub width: ChannelWidth,
    pub ipv4: Ipv4Addr,
    pub ipv6_link_local: Option<Ipv6Addr>,
    pub interface_index: u32,
}

#[derive(Debug)]
pub struct AccessPoint {
    config: AccessPointConfig,
    status: AccessPointStatus,
    hostapd: Child,
    dnsmasq: Child,
}

impl AccessPoint {
    pub fn start(mut config: AccessPointConfig) -> Result<Self> {
        config.validate()?;
        fs::create_dir_all(&config.run_dir)
            .map_err(|error| Error::io_context("creating the access point run directory", error))?;
        run("iw", &["reg", "set", &config.country])?;
        run("ip", &["link", "set", &config.interface, "down"])?;
        run("ip", &["addr", "flush", "dev", &config.interface])?;
        run(
            "ip",
            &[
                "addr",
                "add",
                &format!("{}/24", config.address),
                "dev",
                &config.interface,
            ],
        )?;

        let requested_band = config.band();
        let mut last_error = None;
        let mut started = None;
        for (channel, width) in start_attempts(&config) {
            config.channel = channel;
            fs::write(config.hostapd_path(), render_hostapd(&config, width))
                .map_err(|error| Error::io_context("writing hostapd.conf", error))?;
            match spawn_hostapd(&config.hostapd_path()) {
                Ok(child) => {
                    started = Some((child, width));
                    break;
                }
                Err(error) => {
                    tracing::warn!(channel, ?width, %error, "hostapd refused the network; trying the next option");
                    last_error = Some(error);
                }
            }
        }
        let Some((hostapd, width)) = started else {
            return Err(last_error.unwrap_or(Error::AccessPointStart("no channel was attempted")));
        };
        if config.band() == Band::Ghz2_4 {
            if requested_band == Band::Ghz5 {
                tracing::warn!(
                    "the Wi-Fi adapter cannot host the selected 5 GHz network; using 2.4 GHz, which wireless Android Auto may refuse"
                );
            } else {
                tracing::info!("hosting a 2.4 GHz network, which wireless Android Auto may refuse");
            }
        }

        fs::write(config.dnsmasq_path(), render_dnsmasq(&config))
            .map_err(|error| Error::io_context("writing dnsmasq.conf", error))?;
        let dnsmasq = Command::new("dnsmasq")
            .args(dnsmasq_args(&config.dnsmasq_path()))
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| Error::io_context("starting dnsmasq", error))?;

        let status = AccessPointStatus {
            bssid: interface_mac(&config.interface)?,
            width,
            ipv4: config.address,
            ipv6_link_local: interface_link_local(&config.interface),
            interface_index: nix::net::if_::if_nametoindex(config.interface.as_str()).map_err(
                |error| Error::io_context("resolving the AP interface index", error.into()),
            )?,
        };
        tracing::info!(
            ssid = config.ssid,
            channel = config.channel,
            ?width,
            bssid = status.bssid,
            "wireless projection access point is up"
        );
        Ok(Self {
            config,
            status,
            hostapd,
            dnsmasq,
        })
    }

    pub const fn config(&self) -> &AccessPointConfig {
        &self.config
    }

    pub const fn status(&self) -> &AccessPointStatus {
        &self.status
    }

    pub fn stations(&self) -> StationControl {
        StationControl {
            control: ControlSocket::new(
                self.config
                    .run_dir
                    .join("hostapd")
                    .join(&self.config.interface),
                self.config.run_dir.clone(),
                "hostapd",
            ),
        }
    }

    pub fn refresh_link_local(&mut self) -> Option<Ipv6Addr> {
        if self.status.ipv6_link_local.is_none() {
            self.status.ipv6_link_local = interface_link_local(&self.config.interface);
        }
        self.status.ipv6_link_local
    }

    pub fn check(&mut self) -> Result<()> {
        for (name, child) in [
            ("hostapd", &mut self.hostapd),
            ("dnsmasq", &mut self.dnsmasq),
        ] {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| Error::io_context("polling access point processes", error))?
            {
                return Err(Error::ProcessExited {
                    name,
                    status: status.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl Drop for AccessPoint {
    fn drop(&mut self) {
        for child in [&mut self.dnsmasq, &mut self.hostapd] {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("ip")
            .args(["addr", "flush", "dev", &self.config.interface])
            .status();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationControl {
    control: ControlSocket,
}

impl StationControl {
    pub fn deny(&self, station: &str) -> Result<()> {
        let station = valid_station(station)?;
        self.command(&format!("DENY_ACL ADD_MAC {station}"))?;
        self.command(&format!("DEAUTHENTICATE {station}"))
    }

    pub fn admit(&self, station: &str) -> Result<()> {
        let station = valid_station(station)?;
        self.command(&format!("DENY_ACL DEL_MAC {station}"))
    }

    pub fn deauthenticate(&self, station: &str) -> Result<()> {
        let station = valid_station(station)?;
        self.command(&format!("DEAUTHENTICATE {station}"))
    }

    fn command(&self, command: &str) -> Result<()> {
        let reply = self.control.request(command)?;
        if reply.starts_with("OK") {
            Ok(())
        } else {
            Err(Error::CommandFailed {
                command: format!("hostapd {command}"),
                status: reply.trim().to_owned(),
            })
        }
    }
}

fn valid_station(station: &str) -> Result<&str> {
    let valid = station.len() == 17
        && station.bytes().enumerate().all(|(index, byte)| {
            if index % 3 == 2 {
                byte == b':'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    if valid {
        Ok(station)
    } else {
        Err(Error::InvalidConfig("station MAC address"))
    }
}

pub fn station_for(interface: &str, address: Ipv4Addr) -> Option<String> {
    let table = fs::read_to_string("/proc/net/arp").ok()?;
    station_in_arp_table(&table, interface, address)
}

pub fn arp_station_count(interface: &str) -> Option<usize> {
    let table = fs::read_to_string("/proc/net/arp").ok()?;
    Some(count_arp_stations(&table, interface))
}

pub fn station_link_summary(interface: &str, station: &str) -> Option<String> {
    let station = valid_station(station).ok()?;
    let output = Command::new("iw")
        .args(["dev", interface, "station", "get", station])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let summary = summarize_station(&String::from_utf8_lossy(&output.stdout));
    (!summary.is_empty()).then_some(summary)
}

fn summarize_station(report: &str) -> String {
    const FIELDS: [&str; 17] = [
        "DTIM period",
        "beacon interval",
        "last ack signal",
        "avg ack signal",
        "inactive time",
        "rx packets",
        "tx packets",
        "tx retries",
        "tx failed",
        "rx drop misc",
        "signal",
        "signal avg",
        "tx bitrate",
        "rx bitrate",
        "beacon loss",
        "connected time",
        "airtime weight",
    ];
    report
        .lines()
        .filter_map(|line| {
            let (name, value) = line.trim().split_once(':')?;
            let name = name.trim();
            FIELDS
                .contains(&name)
                .then(|| format!("{}={}", name.replace(' ', "_"), value.trim()))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

const RADIO_DEBUG_ROOT: &str = "/sys/kernel/debug/ieee80211";
const RADIO_DEBUG_FILES: [&str; 4] = [
    "runtime-pm",
    "deep-sleep",
    "idle-timeout",
    "runtime_pm_stats",
];
const STATION_DEBUG_FILES: [&str; 3] = ["flags", "num_ps_buf_frames", "airtime"];
const RADIO_DEBUG_VALUE_LIMIT: usize = 300;

fn radio_debug_directories(interface: &str, station: &str) -> Option<(PathBuf, PathBuf)> {
    let station = valid_station(station).ok()?;
    let plain = |name: &str| {
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };
    if !plain(interface) {
        return None;
    }
    let radio = fs::read_to_string(format!("/sys/class/net/{interface}/phy80211/name")).ok()?;
    let radio = radio.trim();
    if !plain(radio) {
        return None;
    }
    let root = Path::new(RADIO_DEBUG_ROOT).join(radio);
    let stations = root
        .join(format!("netdev:{interface}"))
        .join("stations")
        .join(station);
    Some((root.join("mt76"), stations))
}

pub fn radio_debug_summary(interface: &str, station: &str) -> Option<String> {
    let (radio, station) = radio_debug_directories(interface, station)?;
    let mut values = Vec::new();
    for (directory, names, prefix) in [
        (&radio, &RADIO_DEBUG_FILES[..], ""),
        (&station, &STATION_DEBUG_FILES[..], "station_"),
    ] {
        for name in names {
            if let Ok(text) = fs::read_to_string(directory.join(name)) {
                values.push(format!("{prefix}{name}={}", single_line(&text)));
            }
        }
    }
    (!values.is_empty()).then(|| values.join("; "))
}

pub fn radio_debug_names(interface: &str, station: &str) -> Option<String> {
    let (radio, station) = radio_debug_directories(interface, station)?;
    let names = |directory: &Path| -> String {
        let mut names: Vec<String> = fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        names.join(" ")
    };
    Some(format!(
        "radio: {}; station: {}",
        names(&radio),
        names(&station)
    ))
}

fn single_line(text: &str) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() > RADIO_DEBUG_VALUE_LIMIT {
        let mut end = RADIO_DEBUG_VALUE_LIMIT;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}

fn station_in_arp_table(table: &str, interface: &str, address: Ipv4Addr) -> Option<String> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [ip, _, flags, mac, _, device] = fields[..] else {
            return None;
        };
        (device == interface
            && ip.parse() == Ok(address)
            && flags != "0x0"
            && mac != "00:00:00:00:00:00")
            .then(|| mac.to_ascii_lowercase())
            .filter(|mac| valid_station(mac).is_ok())
    })
}

fn count_arp_stations(table: &str, interface: &str) -> usize {
    table
        .lines()
        .skip(1)
        .filter(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [_, _, flags, mac, _, device] = fields[..] else {
                return false;
            };
            device == interface
                && flags != "0x0"
                && mac != "00:00:00:00:00:00"
                && valid_station(mac).is_ok()
        })
        .count()
}

fn start_attempts(config: &AccessPointConfig) -> Vec<(u8, ChannelWidth)> {
    let mut attempts = Vec::new();
    let mut width = Some(config.width.min(ChannelWidth::widest_for(config.channel)));
    while let Some(current) = width {
        attempts.push((config.channel, current));
        width = current.narrower();
    }
    if config.channel != FALLBACK_2GHZ_CHANNEL {
        attempts.push((FALLBACK_2GHZ_CHANNEL, ChannelWidth::Mhz20));
    }
    attempts
}

pub fn detect_interface() -> Option<String> {
    detect_interface_in(Path::new("/sys/class/net"))
}

fn detect_interface_in(sysfs_net: &Path) -> Option<String> {
    let mut candidates: Vec<(bool, String)> = fs::read_dir(sysfs_net)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry.path().join("phy80211").exists() || entry.path().join("wireless").exists()
        })
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let on_usb = fs::canonicalize(entry.path().join("device")).is_ok_and(|device| {
                device
                    .components()
                    .any(|part| part.as_os_str().to_string_lossy().starts_with("usb"))
            });
            Some((!on_usb, name))
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next().map(|(_, name)| name)
}

fn spawn_hostapd(config_path: &Path) -> Result<Child> {
    let mut child = Command::new("hostapd")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::io_context("starting hostapd", error))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(Error::Protocol("hostapd stdout was not captured"))?;
    let (ready_sender, ready) = mpsc::channel();
    thread::Builder::new()
        .name("hostapd-log".into())
        .spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if line.contains("AP-ENABLED") {
                    let _ = ready_sender.send(true);
                } else if line.contains("AP-DISABLED") || line.contains("Could not set channel") {
                    let _ = ready_sender.send(false);
                }
                tracing::debug!(target: "hostapd", "{line}");
            }
            let _ = ready_sender.send(false);
        })
        .map_err(|error| Error::io_context("spawning the hostapd log reader", error))?;
    match ready.recv_timeout(HOSTAPD_START_TIMEOUT) {
        Ok(true) => Ok(child),
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            Err(Error::AccessPointStart(match outcome {
                Ok(_) => "hostapd could not enable the access point",
                Err(_) => "hostapd did not report AP-ENABLED in time",
            }))
        }
    }
}

pub(crate) fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|error| Error::io_context("running an access point helper", error))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: format!("{program} {}", args.join(" ")),
            status: status.to_string(),
        })
    }
}

fn interface_mac(interface: &str) -> Result<String> {
    fs::read_to_string(Path::new("/sys/class/net").join(interface).join("address"))
        .map(|address| address.trim().to_ascii_lowercase())
        .map_err(|error| Error::io_context("reading the AP interface MAC", error))
}

fn interface_link_local(interface: &str) -> Option<Ipv6Addr> {
    nix::ifaddrs::getifaddrs().ok()?.find_map(|entry| {
        (entry.interface_name == interface)
            .then(|| entry.address?.as_sockaddr_in6().map(|address| address.ip()))
            .flatten()
            .filter(Ipv6Addr::is_unicast_link_local)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AccessPointConfig {
        AccessPointConfig {
            interface: "wlan0".into(),
            ssid: "Prelude-1A2B".into(),
            passphrase: "abcdefgh12345678".into(),
            country: "CA".into(),
            channel: 36,
            width: ChannelWidth::Mhz80,
            address: Ipv4Addr::new(10, 47, 0, 1),
            run_dir: PathBuf::from("/run/prelude/wireless"),
        }
    }

    #[test]
    fn arp_lookup_finds_the_complete_entry_on_the_ap_interface() {
        let table = "IP address       HW type     Flags       HW address            Mask     Device\n\
                     10.47.0.20       0x1         0x0         00:00:00:00:00:00     *        wlan0\n\
                     10.47.0.21       0x1         0x2         AA:BB:CC:DD:EE:01     *        eth0\n\
                     10.47.0.21       0x1         0x2         AA:BB:CC:DD:EE:02     *        wlan0\n";
        let station = |octet| station_in_arp_table(table, "wlan0", Ipv4Addr::new(10, 47, 0, octet));
        assert_eq!(station(21).as_deref(), Some("aa:bb:cc:dd:ee:02"));
        assert_eq!(station(20), None);
        assert_eq!(station(22), None);
        assert_eq!(count_arp_stations(table, "wlan0"), 1);
        assert_eq!(count_arp_stations(table, "eth0"), 1);
    }

    #[test]
    fn station_summary_keeps_the_link_counters_on_one_line() {
        let report = "Station aa:bb:cc:dd:ee:02 (on wlan0)\n\
                      \tinactive time:\t12 ms\n\
                      \trx bytes:\t123456\n\
                      \ttx retries:\t42\n\
                      \ttx failed:\t1\n\
                      \tsignal:  \t-48 [-50, -52] dBm\n\
                      \ttx bitrate:\t270.0 MBit/s MCS 15 40MHz short GI\n\
                      \tauthorized:\tyes\n";
        assert_eq!(
            summarize_station(report),
            "inactive_time=12 ms; tx_retries=42; tx_failed=1; signal=-48 [-50, -52] dBm; \
             tx_bitrate=270.0 MBit/s MCS 15 40MHz short GI"
        );
        assert_eq!(
            summarize_station("command failed: No such file or directory (-2)\n"),
            ""
        );
    }

    #[test]
    fn radio_debug_values_fit_on_one_line_and_bad_names_are_refused() {
        assert_eq!(
            single_line("awake time:     1200\ndoze time:        30\nlow power wakes:  4\n"),
            "awake time: 1200 doze time: 30 low power wakes: 4"
        );
        let long = "é".repeat(RADIO_DEBUG_VALUE_LIMIT);
        let cut = single_line(&long);
        assert!(cut.len() <= RADIO_DEBUG_VALUE_LIMIT);
        assert!(cut.chars().all(|character| character == 'é'));
        assert_eq!(radio_debug_summary("../wlan0", "aa:bb:cc:dd:ee:ff"), None);
        assert_eq!(radio_debug_names("wlan0", "not-a-mac"), None);
    }

    #[test]
    fn station_commands_reject_malformed_addresses() {
        assert!(valid_station("aa:bb:cc:dd:ee:ff").is_ok());
        assert!(valid_station("aa:bb:cc:dd:ee:ff\nQUIT").is_err());
        assert!(valid_station("aa-bb-cc-dd-ee-ff").is_err());
        assert!(valid_station("not-a-mac").is_err());
    }

    #[test]
    fn valid_config_passes() {
        config().validate().expect("valid");
        assert_eq!(config().frequency_mhz(), 5180);
    }

    #[test]
    fn accepts_every_2ghz_and_non_dfs_5ghz_channel() {
        for &channel in CHANNELS_2GHZ.iter().chain(&CHANNELS_5GHZ) {
            let config = AccessPointConfig {
                channel,
                ..config()
            };
            config.validate().expect("supported channel");
        }
    }

    #[test]
    fn rejects_dfs_and_out_of_band_channels() {
        for channel in [0, 14, 32, 52, 64, 100, 144, 169, 177] {
            let config = AccessPointConfig {
                channel,
                ..config()
            };
            assert!(config.validate().is_err(), "channel {channel}");
        }
    }

    #[test]
    fn rejects_bad_credentials_and_injection() {
        for config in [
            AccessPointConfig {
                passphrase: "short".into(),
                ..config()
            },
            AccessPointConfig {
                ssid: "a\ninterface=eth0".into(),
                ..config()
            },
            AccessPointConfig {
                interface: "wlan0 eth0".into(),
                ..config()
            },
            AccessPointConfig {
                country: "ca".into(),
                ..config()
            },
            AccessPointConfig {
                address: Ipv4Addr::new(8, 8, 8, 1),
                ..config()
            },
        ] {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }

    #[test]
    fn hostapd_80mhz_uses_the_block_center() {
        let rendered = render_hostapd(&config(), ChannelWidth::Mhz80);
        assert!(rendered.contains("hw_mode=a\n"));
        assert!(rendered.contains("channel=36\n"));
        assert!(rendered.contains("ht_capab=[HT40+][SHORT-GI-20][SHORT-GI-40]\n"));
        assert!(rendered.contains("vht_oper_chwidth=1\n"));
        assert!(rendered.contains("vht_oper_centr_freq_seg0_idx=42\n"));
        assert!(rendered.contains("country_code=CA\n"));
        assert!(rendered.contains("wpa=2\n"));
        assert!(rendered.contains("disassoc_low_ack=0\n"));
        assert!(rendered.contains("beacon_int=50\n"));
        assert!(rendered.contains("dtim_period=1\n"));

        let upper = AccessPointConfig {
            channel: 153,
            ..config()
        };
        let rendered = render_hostapd(&upper, ChannelWidth::Mhz80);
        assert!(rendered.contains("[HT40-]"));
        assert!(rendered.contains("vht_oper_centr_freq_seg0_idx=155\n"));
    }

    #[test]
    fn narrower_widths_drop_vht() {
        let rendered = render_hostapd(&config(), ChannelWidth::Mhz20);
        assert!(!rendered.contains("ieee80211ac"));
        assert!(!rendered.contains("HT40"));
        assert_eq!(ChannelWidth::Mhz80.narrower(), Some(ChannelWidth::Mhz40));
        assert_eq!(ChannelWidth::Mhz20.narrower(), None);
    }

    #[test]
    fn two_point_four_ghz_uses_hw_mode_g_and_caps_at_40mhz() {
        let config = AccessPointConfig {
            channel: 6,
            ..config()
        };
        config.validate().expect("valid");
        assert_eq!(config.frequency_mhz(), 2437);
        let rendered = render_hostapd(&config, ChannelWidth::Mhz20);
        assert!(rendered.contains("hw_mode=g\n"));
        assert!(!rendered.contains("HT40"));
        let rendered = render_hostapd(&config, ChannelWidth::Mhz80);
        assert!(rendered.contains("ht_capab=[HT40+][SHORT-GI-20][SHORT-GI-40]\n"));
        assert!(!rendered.contains("ieee80211ac"));
        let upper = AccessPointConfig {
            channel: 11,
            ..config
        };
        assert!(render_hostapd(&upper, ChannelWidth::Mhz40).contains("[HT40-]"));
    }

    #[test]
    fn channel_165_is_20mhz_only() {
        let config = AccessPointConfig {
            channel: 165,
            ..config()
        };
        assert_eq!(ChannelWidth::supported_on(165), [ChannelWidth::Mhz20]);
        let rendered = render_hostapd(&config, ChannelWidth::Mhz80);
        assert!(rendered.contains("ht_capab=[SHORT-GI-20]\n"));
        assert!(!rendered.contains("ieee80211ac"));
        assert_eq!(
            start_attempts(&config),
            vec![
                (165, ChannelWidth::Mhz20),
                (FALLBACK_2GHZ_CHANNEL, ChannelWidth::Mhz20)
            ]
        );
    }

    #[test]
    fn supported_widths_follow_the_band() {
        assert_eq!(ChannelWidth::supported_on(36), ChannelWidth::ALL);
        assert_eq!(
            ChannelWidth::supported_on(1),
            [ChannelWidth::Mhz20, ChannelWidth::Mhz40]
        );
        for width in ChannelWidth::ALL {
            assert_eq!(ChannelWidth::from_mhz(width.mhz()), Some(width));
        }
        assert_eq!(ChannelWidth::from_mhz(160), None);
    }

    #[test]
    fn five_ghz_attempts_narrow_then_fall_back_to_2ghz() {
        assert_eq!(
            start_attempts(&config()),
            vec![
                (36, ChannelWidth::Mhz80),
                (36, ChannelWidth::Mhz40),
                (36, ChannelWidth::Mhz20),
                (FALLBACK_2GHZ_CHANNEL, ChannelWidth::Mhz20),
            ]
        );
        let config = AccessPointConfig {
            channel: 11,
            width: ChannelWidth::Mhz20,
            ..config()
        };
        assert_eq!(
            start_attempts(&config),
            vec![
                (11, ChannelWidth::Mhz20),
                (FALLBACK_2GHZ_CHANNEL, ChannelWidth::Mhz20)
            ]
        );
        let config = AccessPointConfig {
            channel: FALLBACK_2GHZ_CHANNEL,
            ..config
        };
        assert_eq!(
            start_attempts(&config),
            vec![(FALLBACK_2GHZ_CHANNEL, ChannelWidth::Mhz20)]
        );
    }

    #[test]
    fn interface_detection_prefers_usb_adapters() {
        let root = tempfile::tempdir().expect("tempdir");
        let devices = root.path().join("devices");
        let net = root.path().join("net");
        for (name, wireless, bus) in [
            ("eth0", false, "platform"),
            ("wlan0", true, "platform"),
            ("wlx00c0ca", true, "usb1"),
        ] {
            let device = devices.join(bus).join(name);
            fs::create_dir_all(&device).expect("device");
            let interface = net.join(name);
            fs::create_dir_all(&interface).expect("interface");
            std::os::unix::fs::symlink(&device, interface.join("device")).expect("link");
            if wireless {
                fs::create_dir_all(interface.join("phy80211")).expect("phy");
            }
        }
        assert_eq!(detect_interface_in(&net).as_deref(), Some("wlx00c0ca"));
        fs::remove_dir_all(net.join("wlx00c0ca")).expect("unplug");
        assert_eq!(detect_interface_in(&net).as_deref(), Some("wlan0"));
        fs::remove_dir_all(net.join("wlan0")).expect("unplug");
        assert_eq!(detect_interface_in(&net), None);
    }

    #[test]
    fn dnsmasq_serves_the_ap_subnet_without_dns_or_route() {
        let rendered = render_dnsmasq(&config());
        assert!(rendered.contains("interface=wlan0\n"));
        assert!(rendered.contains("port=0\n"));
        assert!(rendered.contains("dhcp-range=10.47.0.10,10.47.0.50,255.255.255.0,12h\n"));
        assert!(rendered.contains("dhcp-option=option:router\n"));
        assert!(rendered.contains("leasefile-ro\n"));
        assert!(rendered.contains("pid-file=\n"));
    }

    #[test]
    fn dnsmasq_conf_file_is_attached_to_its_option() {
        assert_eq!(
            dnsmasq_args(&config().dnsmasq_path()),
            [
                OsString::from("--keep-in-foreground"),
                OsString::from("--conf-file=/run/prelude/wireless/dnsmasq.conf"),
            ]
        );
    }
}
