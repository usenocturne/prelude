//! Checking for and installing Prelude updates from GitHub releases.
//!
//! The Updates page sends commands to one worker thread and polls its state,
//! in the same pattern as [`crate::internet`]. Release selection, slot guards
//! and the bundle manifest check are pure functions; GitHub, `swupdate` and
//! the reboot sit behind traits so the worker can be tested with fakes.

pub mod io;
pub mod release;
pub mod system;

use std::{
    fs, io as stdio,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

pub use release::{Asset, Candidate, Flavour, Relation, Release};
use tracing::{info, warn};

use self::{
    io::{Installer, Network, ReleaseSource},
    release::{candidates, newest_update, parse_releases},
    system::{
        Pending, SystemPaths, bundle_version, clear_pending, install_guard, pending_update,
        write_pending,
    },
};
use crate::settings::UpdateChannel;

/// Space kept free beyond the bundle itself.
const SPACE_MARGIN: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCommand {
    Check {
        channel: UpdateChannel,
    },
    ListAll {
        channel: UpdateChannel,
    },
    Install(Candidate),
    /// Stops connecting, checking or downloading; ignored once installing.
    Cancel,
    Restart,
    /// Returns a finished check or a failure to `Idle`.
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Unavailable(UpdateError),
    Connecting,
    Checking,
    UpToDate,
    Available(Candidate),
    Versions(Vec<Candidate>),
    Downloading {
        version: String,
        done: u64,
        total: u64,
    },
    Verifying,
    Installing {
        version: String,
    },
    Installed {
        version: String,
    },
    /// The restart has been asked for; the screen holds until power-off.
    Restarting,
    Failed(UpdateError),
}

/// Removes downloads that an earlier run left behind (power lost or the
/// daemon stopped mid-download) and a half-written pending marker. Nothing
/// is downloading when the updater starts, so all of it is stale.
fn sweep_leftovers(state_dir: &Path) {
    if let Ok(entries) = fs::read_dir(state_dir.join("ota")) {
        for entry in entries.flatten() {
            if let Err(error) = fs::remove_file(entry.path()) {
                warn!(%error, path = %entry.path().display(), "could not remove a leftover download");
            }
        }
    }
    let _ = fs::remove_file(state_dir.join("pending-update.tmp"));
}

/// preluded runs at real-time priority for media. Downloading, hashing and
/// `swupdate` (which inherits this thread's scheduling) must not compete
/// with it, so the updater thread drops to normal scheduling at the lowest
/// priority.
fn run_in_background() {
    use thread_priority::{
        NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy,
        set_thread_priority_and_policy, thread_native_id,
    };
    if let Err(error) = set_thread_priority_and_policy(
        thread_native_id(),
        ThreadPriority::Min,
        ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other),
    ) {
        warn!(?error, "could not lower the updater's priority");
    }
}

/// Cloneable handle to the updater worker.
#[derive(Debug, Clone)]
pub struct Updater {
    state: Arc<Mutex<UpdateState>>,
    commands: Sender<UpdateCommand>,
    cancel: Arc<AtomicBool>,
}

impl Updater {
    pub fn start(
        source: Box<dyn ReleaseSource>,
        installer: Box<dyn Installer>,
        network: Box<dyn Network>,
        paths: SystemPaths,
        repository: String,
        installed: String,
    ) -> stdio::Result<Self> {
        let initial = if paths.ota_disabled() || paths.flavour().is_none() {
            UpdateState::Unavailable(UpdateError::Unavailable)
        } else {
            UpdateState::Idle
        };
        sweep_leftovers(&paths.state_dir);
        let initial = match system::reconcile_pending(
            &paths.state_dir,
            &paths.cmdline,
            system::current_boot().as_deref(),
            (paths.active_slot)(),
        ) {
            system::Reconciled::FellBack { version } if initial == UpdateState::Idle => {
                warn!(
                    version,
                    "the last update did not start; the previous version is running"
                );
                UpdateState::Failed(UpdateError::DidNotStart)
            }
            system::Reconciled::Removed => {
                info!("removed the marker of an install that never switched slots");
                initial
            }
            _ => initial,
        };
        let state = Arc::new(Mutex::new(initial));
        let cancel = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::channel();
        let worker = Worker {
            source,
            installer,
            network,
            paths,
            repository,
            installed,
            state: Arc::clone(&state),
            cancel: Arc::clone(&cancel),
        };
        thread::Builder::new()
            .name("prelude-updater".to_owned())
            .spawn(move || {
                run_in_background();
                worker.run(&receiver);
            })?;
        Ok(Self {
            state,
            commands,
            cancel,
        })
    }

    pub fn send(&self, command: UpdateCommand) {
        if command == UpdateCommand::Cancel {
            // The worker may be busy downloading; it polls this flag.
            self.cancel.store(true, Ordering::SeqCst);
            return;
        }
        if self.commands.send(command).is_err() {
            warn!("the updater has stopped");
        }
    }

    pub fn state(&self) -> UpdateState {
        self.state
            .lock()
            .map_or(UpdateState::Idle, |state| state.clone())
    }

    /// A handle with no worker, for UI tests.
    #[cfg(test)]
    pub fn detached() -> (Self, Receiver<UpdateCommand>, Arc<AtomicBool>) {
        let (commands, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        (
            Self {
                state: Arc::new(Mutex::new(UpdateState::Idle)),
                commands,
                cancel: Arc::clone(&cancel),
            },
            receiver,
            cancel,
        )
    }

    #[cfg(test)]
    pub fn set_state(&self, state: UpdateState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }
}

struct Worker {
    source: Box<dyn ReleaseSource>,
    installer: Box<dyn Installer>,
    network: Box<dyn Network>,
    paths: SystemPaths,
    repository: String,
    installed: String,
    state: Arc<Mutex<UpdateState>>,
    cancel: Arc<AtomicBool>,
}

impl Worker {
    fn run(self, commands: &Receiver<UpdateCommand>) {
        while let Ok(command) = commands.recv() {
            if matches!(self.state(), UpdateState::Unavailable(_)) {
                continue;
            }
            self.cancel.store(false, Ordering::SeqCst);
            match command {
                UpdateCommand::Check { channel } => self.check(channel, false),
                UpdateCommand::ListAll { channel } => self.check(channel, true),
                UpdateCommand::Install(candidate) => self.install(&candidate),
                UpdateCommand::Restart => {
                    let previous = self.state();
                    let installed = matches!(previous, UpdateState::Installed { .. });
                    if installed
                        || pending_update(&self.paths.state_dir, &self.paths.cmdline)
                            == Pending::NeedsRestart
                    {
                        self.set(UpdateState::Restarting);
                        if let Err(error) = self.installer.reboot() {
                            warn!(?error, "could not restart");
                            self.set(previous);
                        }
                    }
                }
                UpdateCommand::Dismiss => {
                    if matches!(
                        self.state(),
                        UpdateState::Failed(_)
                            | UpdateState::UpToDate
                            | UpdateState::Available(_)
                            | UpdateState::Versions(_)
                    ) {
                        self.set(UpdateState::Idle);
                    }
                }
                UpdateCommand::Cancel => {}
            }
        }
    }

    fn state(&self) -> UpdateState {
        self.state
            .lock()
            .map_or(UpdateState::Idle, |state| state.clone())
    }

    fn set(&self, state: UpdateState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Records the outcome of a step: `Cancelled` returns to `Idle`.
    fn finish(&self, result: Result<UpdateState, UpdateError>) {
        match result {
            Ok(state) => self.set(state),
            Err(UpdateError::Cancelled) => self.set(UpdateState::Idle),
            Err(error) => {
                info!(reason = error.message(), "update did not go ahead");
                self.set(UpdateState::Failed(error));
            }
        }
    }

    /// Gets online. A connect that was started is handed back even when it
    /// is cancelled or fails, so the access point always returns.
    fn go_online(&self) -> Result<(), UpdateError> {
        self.network.acquire(&|| self.cancelled()).inspect_err(|_| {
            self.network.release();
        })
    }

    fn check(&self, channel: UpdateChannel, list_all: bool) {
        let Some(flavour) = self.paths.flavour() else {
            self.finish(Err(UpdateError::Unavailable));
            return;
        };
        self.set(UpdateState::Connecting);
        if let Err(error) = self.go_online() {
            self.finish(Err(error));
            return;
        }
        self.set(UpdateState::Checking);
        let result = self
            .source
            .releases(&self.repository)
            .and_then(|json| parse_releases(&json))
            .and_then(|releases| {
                let installed = semver::Version::parse(&self.installed).ok();
                candidates(&releases, channel, flavour, installed.as_ref())
            })
            .map(|found| {
                if list_all {
                    UpdateState::Versions(found)
                } else {
                    newest_update(&found)
                        .cloned()
                        .map_or(UpdateState::UpToDate, UpdateState::Available)
                }
            });
        self.network.release();
        self.finish(if self.cancelled() {
            Err(UpdateError::Cancelled)
        } else {
            result
        });
    }

    fn install(&self, candidate: &Candidate) {
        let result = install_guard(&self.paths).and_then(|(slot, _)| {
            let ota_dir = self.paths.state_dir.join("ota");
            fs::create_dir_all(&ota_dir).map_err(|_| UpdateError::DownloadFailed)?;
            let free = (self.paths.free_bytes)(&ota_dir).unwrap_or(0);
            if free < candidate.asset.size.saturating_add(SPACE_MARGIN) {
                return Err(UpdateError::NoSpace);
            }
            self.set(UpdateState::Connecting);
            self.go_online()?;
            let bundle = ota_dir.join(&candidate.asset.name);
            let result = self.download_and_install(candidate, &bundle, slot);
            let _ = fs::remove_file(&bundle);
            self.network.release();
            result
        });
        self.finish(result);
    }

    fn download_and_install(
        &self,
        candidate: &Candidate,
        bundle: &Path,
        slot: system::Slot,
    ) -> Result<UpdateState, UpdateError> {
        let version = candidate.version.to_string();
        let total = candidate.asset.size;
        self.set(UpdateState::Downloading {
            version: version.clone(),
            done: 0,
            total,
        });
        self.source.download(
            &candidate.asset,
            bundle,
            &mut |done| {
                self.set(UpdateState::Downloading {
                    version: version.clone(),
                    done,
                    total,
                });
            },
            &|| self.cancelled(),
        )?;
        self.set(UpdateState::Verifying);
        // The bundle's version carries no build metadata; a tag may.
        let mut expected = candidate.version.clone();
        expected.build = semver::BuildMetadata::EMPTY;
        if bundle_version(bundle)? != expected.to_string() {
            return Err(UpdateError::BadBundle);
        }
        if self.cancelled() {
            return Err(UpdateError::Cancelled);
        }
        write_pending(&self.paths.state_dir, slot, &version).map_err(|error| {
            warn!(%error, "could not record the pending update");
            UpdateError::InstallFailed
        })?;
        self.set(UpdateState::Installing {
            version: version.clone(),
        });
        info!(version, slot = slot.as_str(), "installing update");
        if let Err(error) = self.installer.install(bundle, slot) {
            clear_pending(&self.paths.state_dir);
            return Err(error);
        }
        info!(version, "update installed; restart to finish");
        Ok(UpdateState::Installed { version })
    }
}

/// Why a check or install did not go ahead. [`UpdateError::message`] is the
/// fixed copy the Updates page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    Unavailable,
    WrongFlavour,
    Offline,
    GithubBusy,
    GithubUnreachable,
    NoSpace,
    DownloadFailed,
    BadBundle,
    InstallFailed,
    InstallBusy,
    DidNotStart,
    RestartPending,
    NotHealthy,
    SlotUnknown,
    Cancelled,
}

impl UpdateError {
    pub const fn message(&self) -> &'static str {
        match self {
            Self::Unavailable => "Updates are not available on this build",
            Self::WrongFlavour => "No update for this build type",
            Self::Offline => "Could not get online",
            Self::GithubBusy => "GitHub is busy. Try again later.",
            Self::GithubUnreachable => "Could not reach GitHub",
            Self::NoSpace => "Not enough free space",
            Self::DownloadFailed => "The download failed",
            Self::BadBundle => "The download is not the expected file",
            Self::InstallFailed => "Install failed. Still on this version.",
            Self::InstallBusy => "Another install is already running",
            Self::DidNotStart => "The last update did not start",
            Self::RestartPending => "Restart to finish the last update",
            Self::NotHealthy => "Not ready yet. Try again in a minute.",
            Self::SlotUnknown => "This system's slot could not be read",
            Self::Cancelled => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::{
        io::{Installer, Network, ReleaseSource},
        system::{Slot, SystemPaths, pending_exists, tests::newc, write_pending},
        *,
    };
    use crate::settings::UpdateChannel;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Download {
        Ok,
        Fail,
        UntilCancelled,
    }

    struct FakeSource {
        json: String,
        download: Download,
        bundle: Vec<u8>,
        downloads: Arc<AtomicUsize>,
    }

    impl ReleaseSource for FakeSource {
        fn releases(&self, _repository: &str) -> Result<String, UpdateError> {
            Ok(self.json.clone())
        }

        fn download(
            &self,
            _asset: &Asset,
            dest: &Path,
            progress: &mut dyn FnMut(u64),
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), UpdateError> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            match self.download {
                Download::Ok => {
                    progress(self.bundle.len() as u64);
                    fs::write(dest, &self.bundle).expect("bundle");
                    Ok(())
                }
                Download::Fail => {
                    fs::write(format!("{}.part", dest.display()), b"half").expect("part");
                    let _ = fs::remove_file(format!("{}.part", dest.display()));
                    Err(UpdateError::DownloadFailed)
                }
                Download::UntilCancelled => {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !cancelled() {
                        progress(1);
                        assert!(Instant::now() < deadline, "never cancelled");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(UpdateError::Cancelled)
                }
            }
        }
    }

    #[derive(Default)]
    struct InstallLog {
        installs: Vec<(PathBuf, Slot, Option<String>)>,
        reboots: usize,
    }

    struct FakeInstaller {
        result: Result<(), UpdateError>,
        state_dir: PathBuf,
        log: Arc<Mutex<InstallLog>>,
        hold: Option<Arc<AtomicBool>>,
    }

    impl Installer for FakeInstaller {
        fn install(&self, bundle: &Path, slot: Slot) -> Result<(), UpdateError> {
            let marker = fs::read_to_string(self.state_dir.join("pending-update")).ok();
            self.log
                .lock()
                .expect("log")
                .installs
                .push((bundle.to_path_buf(), slot, marker));
            if let Some(hold) = &self.hold {
                while hold.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            self.result.clone()
        }

        fn reboot(&self) -> Result<(), UpdateError> {
            self.log.lock().expect("log").reboots += 1;
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeNetwork {
        offline: bool,
        acquired: Arc<AtomicUsize>,
        released: Arc<AtomicUsize>,
    }

    impl Network for FakeNetwork {
        fn acquire(&self, _cancelled: &dyn Fn() -> bool) -> Result<(), UpdateError> {
            self.acquired.fetch_add(1, Ordering::SeqCst);
            if self.offline {
                Err(UpdateError::Offline)
            } else {
                Ok(())
            }
        }

        fn release(&self) {
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn release(tag: &str, prerelease: bool) -> String {
        let version = tag.trim_start_matches('v');
        format!(
            r#"{{"tag_name":"{tag}","prerelease":{prerelease},"draft":false,"assets":[{{"name":"prelude_update_prod_v{version}.swu","size":4096,"browser_download_url":"https://example.invalid/{version}"}}]}}"#
        )
    }

    fn bundle(version: &str) -> Vec<u8> {
        let manifest = format!("software =\n{{\n    version = \"{version}+20261114\";\n}};\n");
        newc(&[
            ("sw-description", manifest.as_bytes()),
            ("boot.vfat", b"boot"),
        ])
    }

    struct Rig {
        updater: Updater,
        dir: tempfile::TempDir,
        network: FakeNetwork,
        downloads: Arc<AtomicUsize>,
        log: Arc<Mutex<InstallLog>>,
    }

    struct Setup {
        releases: Vec<String>,
        download: Download,
        bundle_version: &'static str,
        install: Result<(), UpdateError>,
        offline: bool,
        free_bytes: fn(&Path) -> std::io::Result<u64>,
        ota_disabled: bool,
        hold_install: Option<Arc<AtomicBool>>,
        leftovers: bool,
        active_slot: fn() -> Option<Slot>,
        marker: Option<&'static str>,
    }

    impl Default for Setup {
        fn default() -> Self {
            Self {
                releases: vec![release("v1.0.6", false), release("v1.0.5", false)],
                download: Download::Ok,
                bundle_version: "1.0.6",
                install: Ok(()),
                offline: false,
                free_bytes: |_| Ok(u64::MAX),
                ota_disabled: false,
                hold_install: None,
                leftovers: false,
                active_slot: || Some(Slot::B),
                marker: None,
            }
        }
    }

    impl Rig {
        fn new(setup: Setup) -> Self {
            let dir = tempfile::tempdir().expect("dir");
            let etc = dir.path().join("etc");
            let state = dir.path().join("state");
            fs::create_dir_all(&etc).expect("etc");
            fs::create_dir_all(&state).expect("state");
            fs::write(etc.join("flavour"), "prod\n").expect("flavour");
            if setup.ota_disabled {
                fs::write(etc.join("ota-disabled"), "").expect("disabled");
            }
            fs::write(dir.path().join("cmdline"), "superbird.slot=b").expect("cmdline");
            fs::write(dir.path().join("ready"), "").expect("ready");
            if let Some(marker) = setup.marker {
                fs::write(state.join("pending-update"), marker).expect("marker");
            }
            if setup.leftovers {
                fs::create_dir_all(state.join("ota")).expect("ota");
                fs::write(
                    state.join("ota/prelude_update_prod_v1.0.6.swu.part"),
                    "half",
                )
                .expect("part");
                fs::write(state.join("ota/prelude_update_prod_v1.0.4.swu"), "whole")
                    .expect("bundle");
                fs::write(state.join("pending-update.tmp"), "slot=a\n").expect("tmp");
            }
            let paths = SystemPaths {
                etc_prelude: etc,
                cmdline: dir.path().join("cmdline"),
                ready: dir.path().join("ready"),
                state_dir: state.clone(),
                free_bytes: setup.free_bytes,
                active_slot: setup.active_slot,
            };
            let downloads = Arc::new(AtomicUsize::new(0));
            let log = Arc::new(Mutex::new(InstallLog::default()));
            let network = FakeNetwork {
                offline: setup.offline,
                ..FakeNetwork::default()
            };
            let updater = Updater::start(
                Box::new(FakeSource {
                    json: format!("[{}]", setup.releases.join(",")),
                    download: setup.download,
                    bundle: bundle(setup.bundle_version),
                    downloads: Arc::clone(&downloads),
                }),
                Box::new(FakeInstaller {
                    result: setup.install,
                    state_dir: state,
                    log: Arc::clone(&log),
                    hold: setup.hold_install,
                }),
                Box::new(network.clone()),
                paths,
                "usenocturne/prelude".to_owned(),
                "1.0.5".to_owned(),
            )
            .expect("updater");
            Self {
                updater,
                dir,
                network,
                downloads,
                log,
            }
        }

        fn state_dir(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn settle(&self, done: impl Fn(&UpdateState) -> bool) -> UpdateState {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let state = self.updater.state();
                if done(&state) {
                    return state;
                }
                assert!(Instant::now() < deadline, "stuck in {state:?}");
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn released(&self) -> bool {
            self.network.acquired.load(Ordering::SeqCst)
                == self.network.released.load(Ordering::SeqCst)
        }

        fn ota_files(&self) -> usize {
            fs::read_dir(self.state_dir().join("ota")).map_or(0, Iterator::count)
        }

        fn candidate(&self) -> Candidate {
            self.updater.send(UpdateCommand::Check {
                channel: UpdateChannel::Stable,
            });
            let candidate = match self.settle(terminal) {
                UpdateState::Available(candidate) => candidate,
                other => panic!("expected an update, got {other:?}"),
            };
            // Back to Idle so the next settle() sees the next command's result.
            self.updater.send(UpdateCommand::Dismiss);
            self.settle(|state| *state == UpdateState::Idle);
            candidate
        }
    }

    fn terminal(state: &UpdateState) -> bool {
        matches!(
            state,
            UpdateState::UpToDate
                | UpdateState::Available(_)
                | UpdateState::Versions(_)
                | UpdateState::Installed { .. }
                | UpdateState::Failed(_)
                | UpdateState::Unavailable(_)
        )
    }

    #[test]
    fn check_finds_newer_release_and_releases_network() {
        let rig = Rig::new(Setup::default());
        let candidate = rig.candidate();
        assert_eq!(candidate.version.to_string(), "1.0.6");
        assert!(rig.released());
        assert_eq!(rig.network.acquired.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn check_up_to_date() {
        let rig = Rig::new(Setup {
            releases: vec![release("v1.0.5", false), release("v1.0.6-test", true)],
            ..Setup::default()
        });
        rig.updater.send(UpdateCommand::Check {
            channel: UpdateChannel::Stable,
        });
        assert_eq!(rig.settle(terminal), UpdateState::UpToDate);
        rig.updater.send(UpdateCommand::ListAll {
            channel: UpdateChannel::Beta,
        });
        rig.settle(|state| matches!(state, UpdateState::Versions(_)));
        match rig.updater.state() {
            UpdateState::Versions(list) => assert_eq!(list.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn offline_check_fails_without_contacting_github() {
        let rig = Rig::new(Setup {
            offline: true,
            ..Setup::default()
        });
        rig.updater.send(UpdateCommand::Check {
            channel: UpdateChannel::Stable,
        });
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::Offline)
        );
    }

    #[test]
    fn install_happy_path_writes_marker_then_installs() {
        let rig = Rig::new(Setup::default());
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Installed {
                version: "1.0.6".to_owned()
            }
        );
        let log = rig.log.lock().expect("log");
        assert_eq!(log.installs.len(), 1);
        let (bundle, slot, marker) = &log.installs[0];
        assert_eq!(*slot, Slot::A);
        assert!(
            marker
                .as_deref()
                .is_some_and(|marker| marker.starts_with("slot=a\nversion=1.0.6\n"))
        );
        assert!(!bundle.exists(), "the bundle is deleted after install");
        assert_eq!(rig.ota_files(), 0);
        assert!(rig.released());
        assert!(
            pending_exists(&rig.state_dir()),
            "the marker stays until the next healthy boot"
        );
    }

    #[test]
    fn install_failure_clears_marker() {
        let rig = Rig::new(Setup {
            install: Err(UpdateError::InstallFailed),
            ..Setup::default()
        });
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::InstallFailed)
        );
        assert!(!pending_exists(&rig.state_dir()));
        assert_eq!(rig.ota_files(), 0);
        assert!(rig.released());
    }

    #[test]
    fn download_failure_cleans_up_and_releases() {
        let rig = Rig::new(Setup {
            download: Download::Fail,
            ..Setup::default()
        });
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::DownloadFailed)
        );
        assert_eq!(rig.ota_files(), 0);
        assert!(!pending_exists(&rig.state_dir()));
        assert!(rig.log.lock().expect("log").installs.is_empty());
        assert!(rig.released());
    }

    #[test]
    fn insufficient_space_refuses_before_download() {
        let rig = Rig::new(Setup {
            free_bytes: |_| Ok(1024 * 1024),
            ..Setup::default()
        });
        let candidate = rig.candidate();
        let acquired = rig.network.acquired.load(Ordering::SeqCst);
        rig.updater.send(UpdateCommand::Install(candidate));
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::NoSpace)
        );
        assert_eq!(rig.downloads.load(Ordering::SeqCst), 0);
        assert_eq!(
            rig.network.acquired.load(Ordering::SeqCst),
            acquired,
            "not even online"
        );
    }

    #[test]
    fn pending_marker_blocks_install() {
        let rig = Rig::new(Setup::default());
        let candidate = rig.candidate();
        write_pending(&rig.state_dir(), Slot::A, "1.0.6").expect("pending");
        rig.updater.send(UpdateCommand::Install(candidate));
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::RestartPending)
        );
        assert_eq!(rig.downloads.load(Ordering::SeqCst), 0);
        rig.updater.send(UpdateCommand::Restart);
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.log.lock().expect("log").reboots == 0 {
            assert!(Instant::now() < deadline, "no reboot");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn restart_shows_restarting_before_reboot() {
        let rig = Rig::new(Setup::default());
        write_pending(&rig.state_dir(), Slot::A, "1.0.6").expect("pending");
        rig.updater.send(UpdateCommand::Restart);
        rig.settle(|state| *state == UpdateState::Restarting);
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.log.lock().expect("log").reboots == 0 {
            assert!(Instant::now() < deadline, "no reboot");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn restart_during_trial_boot_does_nothing() {
        let rig = Rig::new(Setup::default());
        write_pending(&rig.state_dir(), Slot::B, "1.0.6").expect("pending");
        rig.updater.send(UpdateCommand::Restart);
        rig.updater.send(UpdateCommand::Check {
            channel: UpdateChannel::Stable,
        });
        rig.settle(terminal);
        assert_eq!(rig.log.lock().expect("log").reboots, 0);
    }

    #[test]
    fn failing_to_get_online_still_releases_the_network() {
        let rig = Rig::new(Setup {
            offline: true,
            ..Setup::default()
        });
        rig.updater.send(UpdateCommand::Check {
            channel: UpdateChannel::Stable,
        });
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::Offline)
        );
        assert!(
            rig.released(),
            "a connect that was started is handed back even when it fails"
        );
    }

    #[test]
    fn leftover_downloads_are_removed_when_the_updater_starts() {
        let rig = Rig::new(Setup {
            leftovers: true,
            ..Setup::default()
        });
        assert_eq!(rig.ota_files(), 0);
        assert!(!rig.state_dir().join("pending-update.tmp").exists());
    }

    #[test]
    fn release_tag_with_build_metadata_installs() {
        let rig = Rig::new(Setup {
            releases: vec![release("v1.0.6+abc", false)],
            ..Setup::default()
        });
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        assert!(matches!(
            rig.settle(terminal),
            UpdateState::Installed { .. }
        ));
    }

    #[test]
    fn updater_work_runs_below_realtime_priority() {
        thread::spawn(|| {
            run_in_background();
            let stat = fs::read_to_string("/proc/thread-self/stat").expect("stat");
            let fields: Vec<&str> = stat
                .rsplit_once(") ")
                .expect("comm")
                .1
                .split_whitespace()
                .collect();
            // Fields after the command name start at field 3 (state).
            assert_eq!(fields[41 - 3], "0", "SCHED_OTHER");
            assert!(
                fields[19 - 3].parse::<i32>().expect("nice") > 0,
                "nice raised"
            );
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn a_fallback_after_restart_is_reported_once() {
        let rig = Rig::new(Setup {
            active_slot: || Some(Slot::A),
            marker: Some("slot=a\nversion=1.0.6\nboot=earlier\n"),
            ..Setup::default()
        });
        assert_eq!(
            rig.updater.state(),
            UpdateState::Failed(UpdateError::DidNotStart)
        );
        assert!(!pending_exists(&rig.state_dir()));
        rig.updater.send(UpdateCommand::Dismiss);
        rig.settle(|state| *state == UpdateState::Idle);
    }

    #[test]
    fn restart_needs_an_installed_update() {
        let rig = Rig::new(Setup::default());
        rig.updater.send(UpdateCommand::Restart);
        rig.updater.send(UpdateCommand::Check {
            channel: UpdateChannel::Stable,
        });
        rig.settle(terminal);
        assert_eq!(rig.log.lock().expect("log").reboots, 0);
    }

    #[test]
    fn manifest_version_mismatch_is_rejected_before_marker() {
        let rig = Rig::new(Setup {
            bundle_version: "1.0.4",
            ..Setup::default()
        });
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Failed(UpdateError::BadBundle)
        );
        assert!(!pending_exists(&rig.state_dir()));
        assert!(rig.log.lock().expect("log").installs.is_empty());
        assert_eq!(rig.ota_files(), 0);
    }

    #[test]
    fn cancel_during_download_returns_to_idle() {
        let rig = Rig::new(Setup {
            download: Download::UntilCancelled,
            ..Setup::default()
        });
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        rig.settle(|state| matches!(state, UpdateState::Downloading { .. }));
        rig.updater.send(UpdateCommand::Cancel);
        assert_eq!(
            rig.settle(|state| *state == UpdateState::Idle),
            UpdateState::Idle
        );
        assert!(rig.released());
        assert!(!pending_exists(&rig.state_dir()));
    }

    #[test]
    fn cancel_is_ignored_while_installing() {
        let hold = Arc::new(AtomicBool::new(true));
        let rig = Rig::new(Setup {
            hold_install: Some(Arc::clone(&hold)),
            ..Setup::default()
        });
        let candidate = rig.candidate();
        rig.updater.send(UpdateCommand::Install(candidate));
        rig.settle(|state| matches!(state, UpdateState::Installing { .. }));
        rig.updater.send(UpdateCommand::Cancel);
        hold.store(false, Ordering::SeqCst);
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Installed {
                version: "1.0.6".to_owned()
            }
        );
    }

    #[test]
    fn ota_disabled_image_is_unavailable() {
        let rig = Rig::new(Setup {
            ota_disabled: true,
            ..Setup::default()
        });
        assert_eq!(
            rig.settle(terminal),
            UpdateState::Unavailable(UpdateError::Unavailable)
        );
        rig.updater.send(UpdateCommand::Check {
            channel: UpdateChannel::Stable,
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rig.updater.state(),
            UpdateState::Unavailable(UpdateError::Unavailable)
        );
        assert_eq!(rig.network.acquired.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dismiss_returns_results_to_idle() {
        let rig = Rig::new(Setup::default());
        rig.candidate();
        rig.updater.send(UpdateCommand::Dismiss);
        assert_eq!(
            rig.settle(|state| *state == UpdateState::Idle),
            UpdateState::Idle
        );
    }

    #[test]
    fn messages_are_fixed_copy() {
        for (error, message) in [
            (
                UpdateError::Unavailable,
                "Updates are not available on this build",
            ),
            (UpdateError::WrongFlavour, "No update for this build type"),
            (UpdateError::Offline, "Could not get online"),
            (UpdateError::GithubBusy, "GitHub is busy. Try again later."),
            (UpdateError::GithubUnreachable, "Could not reach GitHub"),
            (UpdateError::NoSpace, "Not enough free space"),
            (UpdateError::DownloadFailed, "The download failed"),
            (
                UpdateError::BadBundle,
                "The download is not the expected file",
            ),
            (
                UpdateError::InstallFailed,
                "Install failed. Still on this version.",
            ),
            (UpdateError::DidNotStart, "The last update did not start"),
            (
                UpdateError::InstallBusy,
                "Another install is already running",
            ),
            (
                UpdateError::RestartPending,
                "Restart to finish the last update",
            ),
            (
                UpdateError::NotHealthy,
                "Not ready yet. Try again in a minute.",
            ),
            (
                UpdateError::SlotUnknown,
                "This system's slot could not be read",
            ),
            (UpdateError::Cancelled, ""),
        ] {
            assert_eq!(error.message(), message);
        }
    }
}
