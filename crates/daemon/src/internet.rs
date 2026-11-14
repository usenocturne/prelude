//! Getting the Car Thing online: over an existing wired route, or by handing
//! the USB Wi-Fi adapter from the projection access point to client mode.
//!
//! One worker thread owns the hand-off. The UI (and later the updater) sends
//! [`InternetCommand`]s and polls [`InternetState`]. While the adapter is in
//! client mode the access point supervisor stays parked ([`ClientGate`]); on
//! release, or any failure, it restarts the access point with its unchanged
//! configuration.

use std::{
    collections::VecDeque,
    fmt, fs, io,
    net::{TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use prelude_wireless::station::{
    Credential, JoinProgress, ScanResult, Station, StationConfig, derive_psk, passphrase_is_valid,
};
use tracing::{info, warn};

use crate::{WirelessConfig, saved_networks};

pub const REACHABILITY_HOST: &str = "api.github.com";
const REACHABILITY_PORT: u16 = 443;
const REACHABILITY_TIMEOUT: Duration = Duration::from_secs(5);
const PARK_TIMEOUT: Duration = Duration::from_secs(5);
const ONLINE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
const LEASE_TIMEOUT: Duration = Duration::from_secs(15);
const CLIENT_RUN_DIR: &str = "/run/prelude/client";

/// Hand-off between the internet worker and the access point supervisor.
#[derive(Debug, Clone, Default)]
pub struct ClientGate {
    requested: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}

impl ClientGate {
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub fn release(&self) {
        self.requested.store(false, Ordering::Release);
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Called by the supervisor: `true` once its access point is down while
    /// client mode is requested.
    pub fn set_parked(&self, parked: bool) {
        self.parked.store(parked, Ordering::Release);
    }

    pub fn wait_parked(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.parked.load(Ordering::Acquire) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Wired { interface: String },
    Wifi { ssid: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternetError {
    NoAdapter,
    AdapterRemoved,
    WrongPassword,
    NotFound,
    Unsupported,
    NoSavedNetwork,
    NoAddress,
    Unreachable,
    Failed(String),
}

impl InternetError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::NoAdapter => "Connect a USB Wi-Fi adapter",
            Self::AdapterRemoved => "Wi-Fi adapter removed",
            Self::WrongPassword => "Wrong password",
            Self::NotFound => "Network not found",
            Self::Unsupported => "This network's security is not supported",
            Self::NoSavedNetwork => "Choose a network",
            Self::NoAddress => "No address from the network",
            Self::Unreachable => "Connected, but the internet is unreachable",
            Self::Failed(_) => "Wi-Fi failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternetState {
    Idle,
    Pausing,
    Scanning,
    Networks(Vec<ScanResult>),
    Joining { ssid: String },
    Online { route: Route, address: String },
    Failed(InternetError),
}

#[derive(Clone, PartialEq, Eq)]
pub enum InternetCommand {
    Scan,
    /// `passphrase: None` joins an open network. On success the derived PSK
    /// is remembered.
    Join {
        ssid: Vec<u8>,
        passphrase: Option<String>,
    },
    /// Joins a remembered network with its stored PSK.
    JoinSaved {
        ssid: Vec<u8>,
    },
    /// Wired route if one exists, otherwise the first saved network in range.
    Connect,
    Release,
}

impl fmt::Debug for InternetCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan => formatter.write_str("Scan"),
            Self::Join { ssid, passphrase } => formatter
                .debug_struct("Join")
                .field("ssid", &String::from_utf8_lossy(ssid))
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
            Self::JoinSaved { ssid } => formatter
                .debug_struct("JoinSaved")
                .field("ssid", &String::from_utf8_lossy(ssid))
                .finish(),
            Self::Connect => formatter.write_str("Connect"),
            Self::Release => formatter.write_str("Release"),
        }
    }
}

pub trait ClientBackend: Send {
    fn wired_route(&self) -> Option<String>;
    fn start(&mut self) -> Result<Box<dyn ClientLink>, InternetError>;
    fn reachable(&self) -> bool;
}

pub trait ClientLink: Send {
    fn scan(&mut self) -> Result<Vec<ScanResult>, InternetError>;
    fn join(&mut self, credential: &Credential) -> Result<(), InternetError>;
    /// Returns the leased address.
    fn lease(&mut self) -> Result<String, InternetError>;
    fn check(&mut self) -> Result<(), InternetError>;
}

/// Cloneable handle to the internet worker.
#[derive(Debug, Clone)]
pub struct Internet {
    state: Arc<Mutex<InternetState>>,
    commands: Sender<InternetCommand>,
}

impl Internet {
    /// Spawns the worker. It stops (releasing client mode) once every handle
    /// is dropped.
    pub fn start(
        backend: Box<dyn ClientBackend>,
        gate: ClientGate,
        state_dir: PathBuf,
    ) -> io::Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(InternetState::Idle));
        let worker = Worker {
            backend,
            gate,
            state_dir,
            state: Arc::clone(&state),
            commands: receiver,
            pending: VecDeque::new(),
            link: None,
        };
        let _: JoinHandle<()> = thread::Builder::new()
            .name("prelude-internet".to_owned())
            .spawn(move || worker.run())?;
        Ok(Self { state, commands })
    }

    pub fn send(&self, command: InternetCommand) {
        if self.commands.send(command).is_err() {
            warn!("the internet worker has stopped");
        }
    }

    pub fn state(&self) -> InternetState {
        self.state
            .lock()
            .map_or(InternetState::Idle, |state| state.clone())
    }

    /// A handle with no worker, for UI tests: commands arrive on the receiver
    /// and the test sets the state.
    #[cfg(test)]
    pub fn detached() -> (Self, Receiver<InternetCommand>) {
        let (commands, receiver) = mpsc::channel();
        (
            Self {
                state: Arc::new(Mutex::new(InternetState::Idle)),
                commands,
            },
            receiver,
        )
    }

    #[cfg(test)]
    pub fn set_state(&self, state: InternetState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }
}

/// Raised inside the worker when a `Release` arrives between steps.
enum Stop {
    Released,
    Failed(InternetError),
}

impl From<InternetError> for Stop {
    fn from(error: InternetError) -> Self {
        Self::Failed(error)
    }
}

struct Worker {
    backend: Box<dyn ClientBackend>,
    gate: ClientGate,
    state_dir: PathBuf,
    state: Arc<Mutex<InternetState>>,
    commands: Receiver<InternetCommand>,
    pending: VecDeque<InternetCommand>,
    link: Option<Box<dyn ClientLink>>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let command = if let Some(command) = self.pending.pop_front() {
                command
            } else if self.link.is_some() {
                match self.commands.recv_timeout(ONLINE_CHECK_INTERVAL) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        self.check_link();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match self.commands.recv() {
                    Ok(command) => command,
                    Err(_) => break,
                }
            };
            let outcome = match command {
                InternetCommand::Release => Err(Stop::Released),
                InternetCommand::Scan => self.scan(),
                InternetCommand::Join { ssid, passphrase } => self.join_new(ssid, passphrase),
                InternetCommand::JoinSaved { ssid } => self.join_saved(&ssid),
                InternetCommand::Connect => self.connect(),
            };
            match outcome {
                Ok(()) => {}
                Err(Stop::Released) => {
                    self.close();
                    self.set(InternetState::Idle);
                }
                Err(Stop::Failed(error)) => {
                    info!(reason = error.message(), "Wi-Fi client mode ended");
                    self.close();
                    self.set(InternetState::Failed(error));
                }
            }
        }
        self.close();
    }

    fn set(&self, state: InternetState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }

    /// Takes queued commands; a `Release` among them stops the current step.
    fn interrupted(&mut self) -> Result<(), Stop> {
        loop {
            match self.commands.try_recv() {
                Ok(InternetCommand::Release) => {
                    self.pending.clear();
                    return Err(Stop::Released);
                }
                Ok(command) => self.pending.push_back(command),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    fn check_link(&mut self) {
        if let Some(link) = self.link.as_mut()
            && let Err(error) = link.check()
        {
            info!(reason = error.message(), "Wi-Fi client mode ended");
            self.close();
            self.set(InternetState::Failed(error));
        }
    }

    fn close(&mut self) {
        self.link = None;
        self.gate.release();
    }

    fn link(&mut self) -> Result<&mut Box<dyn ClientLink>, Stop> {
        if self.link.is_none() {
            self.set(InternetState::Pausing);
            self.gate.request();
            if !self.gate.wait_parked(PARK_TIMEOUT) {
                return Err(
                    InternetError::Failed("the access point did not stop".to_owned()).into(),
                );
            }
            self.interrupted()?;
            self.link = Some(self.backend.start()?);
        }
        self.link
            .as_mut()
            .ok_or_else(|| InternetError::Failed("no Wi-Fi client link".to_owned()).into())
    }

    fn scan(&mut self) -> Result<(), Stop> {
        self.link()?;
        self.set(InternetState::Scanning);
        let results = self.link()?.scan()?;
        self.interrupted()?;
        self.set(InternetState::Networks(results));
        Ok(())
    }

    fn join_new(&mut self, ssid: Vec<u8>, passphrase: Option<String>) -> Result<(), Stop> {
        let psk = match passphrase {
            Some(passphrase) if passphrase_is_valid(&passphrase) => {
                Some(derive_psk(&ssid, &passphrase))
            }
            Some(_) => return Err(InternetError::WrongPassword.into()),
            None => None,
        };
        self.join(&Credential { ssid, psk }, true)
    }

    fn join_saved(&mut self, ssid: &[u8]) -> Result<(), Stop> {
        let saved = saved_networks::load(&self.state_dir)
            .into_iter()
            .find(|saved| saved.ssid == ssid)
            .ok_or(InternetError::NoSavedNetwork)?;
        self.join(
            &Credential {
                ssid: saved.ssid,
                psk: saved.psk,
            },
            true,
        )
    }

    fn connect(&mut self) -> Result<(), Stop> {
        if let Some(interface) = self.backend.wired_route() {
            self.close();
            self.set(InternetState::Online {
                route: Route::Wired { interface },
                address: String::new(),
            });
            return Ok(());
        }
        let saved = saved_networks::load(&self.state_dir);
        if saved.is_empty() {
            return Err(InternetError::NoSavedNetwork.into());
        }
        self.link()?;
        self.set(InternetState::Scanning);
        let visible = self.link()?.scan()?;
        self.interrupted()?;
        let network = saved
            .into_iter()
            .find(|saved| visible.iter().any(|result| result.ssid == saved.ssid))
            .ok_or(InternetError::NotFound)?;
        self.join(
            &Credential {
                ssid: network.ssid,
                psk: network.psk,
            },
            true,
        )
    }

    fn join(&mut self, credential: &Credential, remember: bool) -> Result<(), Stop> {
        self.link()?;
        let ssid = String::from_utf8_lossy(&credential.ssid).into_owned();
        self.set(InternetState::Joining { ssid: ssid.clone() });
        self.link()?.join(credential)?;
        self.interrupted()?;
        let address = self.link()?.lease()?;
        self.interrupted()?;
        if !self.backend.reachable() {
            return Err(InternetError::Unreachable.into());
        }
        self.interrupted()?;
        if remember
            && let Err(error) = saved_networks::remember(
                &self.state_dir,
                saved_networks::SavedNetwork {
                    ssid: credential.ssid.clone(),
                    psk: credential.psk,
                },
            )
        {
            warn!(%error, "could not remember the Wi-Fi network");
        }
        info!(ssid, address, "online over Wi-Fi");
        self.set(InternetState::Online {
            route: Route::Wifi { ssid },
            address,
        });
        Ok(())
    }
}

/// Interface of the default IPv4 route in `/proc/net/route`, ignoring
/// `exclude` (the Wi-Fi adapter).
pub fn default_route_interface(route_table: &str, exclude: &str) -> Option<String> {
    route_table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let interface = fields.next()?;
        let destination = fields.next()?;
        (destination == "00000000" && interface != exclude).then(|| interface.to_owned())
    })
}

/// Real backend: `wpa_supplicant` and `udhcpc` on the configured adapter.
#[derive(Debug)]
pub struct StationBackend {
    wireless: WirelessConfig,
    run_dir: PathBuf,
}

impl StationBackend {
    pub fn new(wireless: &WirelessConfig) -> Self {
        Self {
            wireless: wireless.clone(),
            run_dir: PathBuf::from(CLIENT_RUN_DIR),
        }
    }

    fn interface(&self) -> Option<String> {
        crate::wireless_runtime::wifi_interface(&self.wireless)
    }
}

impl ClientBackend for StationBackend {
    fn wired_route(&self) -> Option<String> {
        let table = fs::read_to_string("/proc/net/route").ok()?;
        let wifi = self.interface().unwrap_or_default();
        default_route_interface(&table, &wifi)
    }

    fn start(&mut self) -> Result<Box<dyn ClientLink>, InternetError> {
        let interface = self.interface().ok_or(InternetError::NoAdapter)?;
        let station = Station::start(StationConfig {
            interface,
            country: self.wireless.country.clone(),
            run_dir: self.run_dir.clone(),
        })
        .map_err(link_error)?;
        Ok(Box::new(StationLink { station }))
    }

    fn reachable(&self) -> bool {
        let Ok(addresses) = (REACHABILITY_HOST, REACHABILITY_PORT).to_socket_addrs() else {
            return false;
        };
        addresses
            .into_iter()
            .any(|address| TcpStream::connect_timeout(&address, REACHABILITY_TIMEOUT).is_ok())
    }
}

#[derive(Debug)]
struct StationLink {
    station: Station,
}

impl ClientLink for StationLink {
    fn scan(&mut self) -> Result<Vec<ScanResult>, InternetError> {
        self.station.scan(SCAN_TIMEOUT).map_err(link_error)
    }

    fn join(&mut self, credential: &Credential) -> Result<(), InternetError> {
        match self
            .station
            .join(credential, JOIN_TIMEOUT)
            .map_err(link_error)?
        {
            JoinProgress::Connected => Ok(()),
            JoinProgress::WrongPassword => Err(InternetError::WrongPassword),
            JoinProgress::Connecting => Err(InternetError::NotFound),
        }
    }

    fn lease(&mut self) -> Result<String, InternetError> {
        let lease = self
            .station
            .lease(LEASE_TIMEOUT)
            .map_err(|error| match link_error(error) {
                InternetError::Failed(_) => InternetError::NoAddress,
                other => other,
            })?;
        Ok(lease
            .address
            .split_once('/')
            .map_or(lease.address.clone(), |(address, _)| address.to_owned()))
    }

    fn check(&mut self) -> Result<(), InternetError> {
        self.station.check().map_err(link_error)
    }
}

fn link_error(error: prelude_wireless::Error) -> InternetError {
    match error {
        prelude_wireless::Error::ProcessExited {
            name: "wifi-adapter",
            ..
        } => InternetError::AdapterRemoved,
        other => InternetError::Failed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use prelude_wireless::station::{Credential, ScanResult, Security, derive_psk};

    use super::*;
    use crate::saved_networks;

    #[derive(Default)]
    struct Script {
        wired: Option<String>,
        scan: Vec<ScanResult>,
        join: Option<InternetError>,
        join_gate: Option<mpsc::Receiver<()>>,
        reachable: bool,
        check: Option<InternetError>,
        calls: Vec<String>,
        links_alive: usize,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Script>>);

    impl Fake {
        fn with(self, edit: impl FnOnce(&mut Script)) -> Self {
            edit(&mut self.0.lock().expect("script"));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.0.lock().expect("script").calls.clone()
        }

        fn links_alive(&self) -> usize {
            self.0.lock().expect("script").links_alive
        }
    }

    struct FakeLink(Fake);

    impl ClientBackend for Fake {
        fn wired_route(&self) -> Option<String> {
            self.0.lock().expect("script").wired.clone()
        }

        fn start(&mut self) -> Result<Box<dyn ClientLink>, InternetError> {
            let mut script = self.0.lock().expect("script");
            script.calls.push("start".to_owned());
            script.links_alive += 1;
            Ok(Box::new(FakeLink(self.clone())))
        }

        fn reachable(&self) -> bool {
            self.0.lock().expect("script").reachable
        }
    }

    impl ClientLink for FakeLink {
        fn scan(&mut self) -> Result<Vec<ScanResult>, InternetError> {
            let mut script = self.0.0.lock().expect("script");
            script.calls.push("scan".to_owned());
            Ok(script.scan.clone())
        }

        fn join(&mut self, credential: &Credential) -> Result<(), InternetError> {
            let gate = {
                let mut script = self.0.0.lock().expect("script");
                script.calls.push(format!(
                    "join {} {}",
                    String::from_utf8_lossy(&credential.ssid),
                    credential
                        .psk
                        .map_or_else(|| "open".to_owned(), hex::encode)
                ));
                script.join_gate.take()
            };
            if let Some(gate) = gate {
                let _ = gate.recv_timeout(Duration::from_secs(5));
            }
            match self.0.0.lock().expect("script").join.clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn lease(&mut self) -> Result<String, InternetError> {
            self.0
                .0
                .lock()
                .expect("script")
                .calls
                .push("lease".to_owned());
            Ok("192.168.1.23".to_owned())
        }

        fn check(&mut self) -> Result<(), InternetError> {
            match self.0.0.lock().expect("script").check.clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    impl Drop for FakeLink {
        fn drop(&mut self) {
            self.0.0.lock().expect("script").links_alive -= 1;
        }
    }

    struct Rig {
        internet: Internet,
        gate: ClientGate,
        fake: Fake,
        state: tempfile::TempDir,
        stop: Arc<AtomicBool>,
    }

    impl Rig {
        fn new(fake: Fake) -> Self {
            let state = tempfile::tempdir().expect("state");
            let gate = ClientGate::default();
            let stop = Arc::new(AtomicBool::new(false));
            // Stands in for the access point supervisor: parks while asked.
            thread::spawn({
                let gate = gate.clone();
                let stop = Arc::clone(&stop);
                move || {
                    while !stop.load(Ordering::Relaxed) {
                        gate.set_parked(gate.requested());
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            });
            let internet = Internet::start(
                Box::new(fake.clone()),
                gate.clone(),
                state.path().to_path_buf(),
            )
            .expect("worker");
            Self {
                internet,
                gate,
                fake,
                state,
                stop,
            }
        }

        fn settle(&self, done: impl Fn(&InternetState) -> bool) -> InternetState {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let state = self.internet.state();
                if done(&state) {
                    return state;
                }
                assert!(Instant::now() < deadline, "stuck in {state:?}");
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn wait_released(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.gate.requested() || self.fake.links_alive() != 0 {
                assert!(Instant::now() < deadline, "client mode never released");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn terminal(state: &InternetState) -> bool {
        matches!(
            state,
            InternetState::Online { .. } | InternetState::Failed(_) | InternetState::Networks(_)
        )
    }

    fn network(ssid: &str) -> ScanResult {
        ScanResult {
            ssid: ssid.as_bytes().to_vec(),
            signal_dbm: -50,
            frequency_mhz: 2437,
            security: Security::Psk,
        }
    }

    #[test]
    fn wired_route_skips_wifi() {
        let rig = Rig::new(Fake::default().with(|script| script.wired = Some("eth0".to_owned())));
        rig.internet.send(InternetCommand::Connect);
        let state = rig.settle(terminal);
        assert!(matches!(
            state,
            InternetState::Online { route: Route::Wired { ref interface }, .. } if interface == "eth0"
        ));
        assert!(rig.fake.calls().is_empty());
        assert!(!rig.gate.requested());
    }

    #[test]
    fn connect_without_saved_network_asks_to_choose() {
        let rig = Rig::new(Fake::default());
        rig.internet.send(InternetCommand::Connect);
        assert_eq!(
            rig.settle(terminal),
            InternetState::Failed(InternetError::NoSavedNetwork)
        );
        rig.wait_released();
    }

    #[test]
    fn join_success_remembers_network() {
        let rig = Rig::new(Fake::default().with(|script| script.reachable = true));
        rig.internet.send(InternetCommand::Join {
            ssid: b"Home".to_vec(),
            passphrase: Some("correct horse".to_owned()),
        });
        let state = rig.settle(terminal);
        assert_eq!(
            state,
            InternetState::Online {
                route: Route::Wifi {
                    ssid: "Home".to_owned()
                },
                address: "192.168.1.23".to_owned(),
            }
        );
        assert!(
            rig.gate.requested(),
            "online keeps the adapter in client mode"
        );
        let saved = saved_networks::load(rig.state.path());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].ssid, b"Home");
        assert_eq!(saved[0].psk, Some(derive_psk(b"Home", "correct horse")));
    }

    #[test]
    fn wrong_password_is_reported_and_not_saved() {
        let rig = Rig::new(Fake::default().with(|script| {
            script.join = Some(InternetError::WrongPassword);
            script.reachable = true;
        }));
        rig.internet.send(InternetCommand::Join {
            ssid: b"Home".to_vec(),
            passphrase: Some("not the password".to_owned()),
        });
        assert_eq!(
            rig.settle(terminal),
            InternetState::Failed(InternetError::WrongPassword)
        );
        rig.wait_released();
        assert!(saved_networks::load(rig.state.path()).is_empty());
    }

    #[test]
    fn unreachable_after_lease_reports_unreachable() {
        let rig = Rig::new(Fake::default());
        rig.internet.send(InternetCommand::Join {
            ssid: b"Captive".to_vec(),
            passphrase: None,
        });
        assert_eq!(
            rig.settle(terminal),
            InternetState::Failed(InternetError::Unreachable)
        );
        rig.wait_released();
    }

    #[test]
    fn release_during_join_restores_access_point() {
        let (release_join, join_gate) = mpsc::channel();
        let rig = Rig::new(Fake::default().with(|script| {
            script.join_gate = Some(join_gate);
            script.reachable = true;
        }));
        rig.internet.send(InternetCommand::Join {
            ssid: b"Home".to_vec(),
            passphrase: Some("correct horse".to_owned()),
        });
        rig.settle(|state| matches!(state, InternetState::Joining { .. }));
        rig.internet.send(InternetCommand::Release);
        release_join.send(()).expect("unblock join");
        rig.wait_released();
        assert_eq!(
            rig.settle(|state| *state == InternetState::Idle),
            InternetState::Idle
        );
        assert!(!rig.fake.calls().contains(&"lease".to_owned()));
        assert!(saved_networks::load(rig.state.path()).is_empty());
    }

    #[test]
    fn adapter_removed_while_online() {
        let rig = Rig::new(Fake::default().with(|script| script.reachable = true));
        rig.internet.send(InternetCommand::Join {
            ssid: b"Home".to_vec(),
            passphrase: None,
        });
        rig.settle(|state| matches!(state, InternetState::Online { .. }));
        rig.fake.0.lock().expect("script").check = Some(InternetError::AdapterRemoved);
        assert_eq!(
            rig.settle(|state| matches!(state, InternetState::Failed(_))),
            InternetState::Failed(InternetError::AdapterRemoved)
        );
        rig.wait_released();
    }

    #[test]
    fn scan_lists_networks_and_saved_network_joins_with_stored_psk() {
        let rig = Rig::new(Fake::default().with(|script| {
            script.scan = vec![network("Home"), network("Other")];
            script.reachable = true;
        }));
        saved_networks::remember(
            rig.state.path(),
            saved_networks::SavedNetwork {
                ssid: b"Home".to_vec(),
                psk: Some([7; 32]),
            },
        )
        .expect("remember");
        rig.internet.send(InternetCommand::Scan);
        assert_eq!(
            rig.settle(terminal),
            InternetState::Networks(vec![network("Home"), network("Other")])
        );
        rig.internet.send(InternetCommand::JoinSaved {
            ssid: b"Home".to_vec(),
        });
        rig.settle(|state| matches!(state, InternetState::Online { .. }));
        assert!(
            rig.fake
                .calls()
                .contains(&format!("join Home {}", "07".repeat(32)))
        );
        assert_eq!(
            rig.fake
                .calls()
                .iter()
                .filter(|call| *call == "start")
                .count(),
            1,
            "the scan's link is reused"
        );
    }

    #[test]
    fn connect_joins_first_saved_network_in_range() {
        let rig = Rig::new(Fake::default().with(|script| {
            script.scan = vec![network("Hotspot"), network("Cafe")];
            script.reachable = true;
        }));
        for ssid in ["Hotspot", "Home"] {
            saved_networks::remember(
                rig.state.path(),
                saved_networks::SavedNetwork {
                    ssid: ssid.as_bytes().to_vec(),
                    psk: Some([1; 32]),
                },
            )
            .expect("remember");
        }
        rig.internet.send(InternetCommand::Connect);
        let state = rig.settle(terminal);
        assert!(matches!(
            state,
            InternetState::Online { route: Route::Wifi { ref ssid }, .. } if ssid == "Hotspot"
        ));
    }

    #[test]
    fn default_route_interface_parses_proc() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
            wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n\
            eth0\t00000000\t01000A0A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
        assert_eq!(
            default_route_interface(table, "wlan0"),
            Some("eth0".to_owned())
        );
        assert_eq!(
            default_route_interface(table, "eth0"),
            Some("wlan0".to_owned())
        );
        let no_default = "Iface\tDestination\tGateway\n\
            usb0\t002FA8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_route_interface(no_default, "wlan0"), None);
    }

    #[test]
    fn client_gate_waits_for_park() {
        let gate = ClientGate::default();
        gate.request();
        assert!(gate.requested());
        assert!(!gate.wait_parked(Duration::from_millis(50)));
        gate.set_parked(true);
        assert!(gate.wait_parked(Duration::from_millis(50)));
        gate.release();
        assert!(!gate.requested());
    }

    #[test]
    fn error_messages_are_fixed_copy() {
        assert_eq!(
            InternetError::NoAdapter.message(),
            "Connect a USB Wi-Fi adapter"
        );
        assert_eq!(
            InternetError::AdapterRemoved.message(),
            "Wi-Fi adapter removed"
        );
        assert_eq!(InternetError::WrongPassword.message(), "Wrong password");
        assert_eq!(InternetError::NotFound.message(), "Network not found");
        assert_eq!(
            InternetError::Unsupported.message(),
            "This network's security is not supported"
        );
        assert_eq!(InternetError::NoSavedNetwork.message(), "Choose a network");
        assert_eq!(
            InternetError::NoAddress.message(),
            "No address from the network"
        );
        assert_eq!(
            InternetError::Unreachable.message(),
            "Connected, but the internet is unreachable"
        );
        assert_eq!(
            InternetError::Failed("anything".to_owned()).message(),
            "Wi-Fi failed"
        );
    }
}
