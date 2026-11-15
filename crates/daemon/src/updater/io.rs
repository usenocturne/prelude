//! The updater's side effects: GitHub over HTTPS, `swupdate`, reboot, and
//! getting online. Each sits behind a trait so the worker is testable.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::{Asset, UpdateError, system::Slot};
use crate::internet::{Internet, InternetCommand, InternetState};

const API_BODY_LIMIT: u64 = 2 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// A download may take this long plus one second per 64 KiB before it is
/// abandoned: generous for a phone hotspot, but never unbounded.
const DOWNLOAD_BASE_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_MIN_RATE: u64 = 64 * 1024;
/// Longest wait for any single network read. A connection that goes
/// silent (the adapter unplugged, the network gone) fails after this
/// instead of running out the whole download budget.
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(20);
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(90);
const ACQUIRE_POLL: Duration = Duration::from_millis(200);

pub trait ReleaseSource: Send {
    /// The raw releases JSON for `owner/repo`.
    fn releases(&self, repository: &str) -> Result<String, UpdateError>;
    /// Downloads `asset` to `dest` (via `dest.part`), checking its size and,
    /// when known, its SHA-256. Leaves no file behind on failure or cancel.
    fn download(
        &self,
        asset: &Asset,
        dest: &Path,
        progress: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), UpdateError>;
}

pub trait Installer: Send {
    fn install(&self, bundle: &Path, slot: Slot) -> Result<(), UpdateError>;
    fn reboot(&self) -> Result<(), UpdateError>;
}

pub trait Network: Send {
    fn acquire(&self, cancelled: &dyn Fn() -> bool) -> Result<(), UpdateError>;
    fn release(&self);
}

/// 403 and 429 are GitHub's rate limits; anything else is `otherwise`.
pub fn map_status(status: u16, otherwise: UpdateError) -> UpdateError {
    if matches!(status, 403 | 429) {
        UpdateError::GithubBusy
    } else {
        otherwise
    }
}

fn request_error(error: ureq::Error, otherwise: UpdateError) -> UpdateError {
    match error {
        ureq::Error::StatusCode(status) => map_status(status, otherwise),
        other => {
            warn!(error = %other, "GitHub request failed");
            otherwise
        }
    }
}

#[derive(Debug)]
pub struct GithubSource {
    user_agent: String,
    read_stall: Duration,
    https_only: bool,
}

impl GithubSource {
    pub fn new(installed_version: &str) -> Self {
        Self {
            user_agent: format!("prelude-updater/{installed_version}"),
            read_stall: READ_STALL_TIMEOUT,
            https_only: true,
        }
    }

    /// Tests serve bundles from a plain-HTTP server on localhost.
    #[cfg(test)]
    const fn allowing_http(mut self) -> Self {
        self.https_only = false;
        self
    }

    #[cfg(test)]
    const fn with_read_stall(mut self, read_stall: Duration) -> Self {
        self.read_stall = read_stall;
        self
    }

    fn agent(&self, body_timeout: Option<Duration>) -> ureq::Agent {
        ureq::Agent::config_builder()
            .user_agent(self.user_agent.as_str())
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_recv_body(body_timeout)
            .timeout_per_read(Some(self.read_stall))
            // Asset URLs and redirects come from untrusted JSON.
            .https_only(self.https_only)
            .build()
            .into()
    }
}

impl ReleaseSource for GithubSource {
    fn releases(&self, repository: &str) -> Result<String, UpdateError> {
        let url = format!("https://api.github.com/repos/{repository}/releases?per_page=100");
        let mut response = self
            .agent(Some(RESPONSE_TIMEOUT))
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(|error| request_error(error, UpdateError::GithubUnreachable))?;
        response
            .body_mut()
            .with_config()
            .limit(API_BODY_LIMIT)
            .read_to_string()
            .map_err(|error| request_error(error, UpdateError::GithubUnreachable))
    }

    fn download(
        &self,
        asset: &Asset,
        dest: &Path,
        progress: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), UpdateError> {
        let partial = PathBuf::from(format!("{}.part", dest.display()));
        let result = self
            .fetch(asset, &partial, progress, cancelled)
            .and_then(|()| fs::rename(&partial, dest).map_err(|_| UpdateError::DownloadFailed));
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result
    }
}

impl GithubSource {
    fn fetch(
        &self,
        asset: &Asset,
        partial: &Path,
        progress: &mut dyn FnMut(u64),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), UpdateError> {
        let timeout = DOWNLOAD_BASE_TIMEOUT + Duration::from_secs(asset.size / DOWNLOAD_MIN_RATE);
        let response = self
            .agent(Some(timeout))
            .get(&asset.url)
            .call()
            .map_err(|error| request_error(error, UpdateError::DownloadFailed))?;
        let mut reader = response.into_body().into_reader();
        let mut file = File::create(partial).map_err(|_| UpdateError::DownloadFailed)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut done = 0_u64;
        loop {
            if cancelled() {
                return Err(UpdateError::Cancelled);
            }
            let read = reader.read(&mut buffer).map_err(|error| {
                warn!(%error, "update download interrupted");
                UpdateError::DownloadFailed
            })?;
            if read == 0 {
                break;
            }
            done += read as u64;
            if done > asset.size {
                return Err(UpdateError::DownloadFailed);
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|_| UpdateError::DownloadFailed)?;
            progress(done);
        }
        if done != asset.size {
            return Err(UpdateError::DownloadFailed);
        }
        if let Some(expected) = asset.sha256
            && <[u8; 32]>::from(hasher.finalize()) != expected
        {
            return Err(UpdateError::BadBundle);
        }
        file.sync_all().map_err(|_| UpdateError::DownloadFailed)
    }
}

/// Runs `swupdate` as a child, inside preluded's sandbox.
#[derive(Debug)]
pub struct SwupdateInstaller {
    pub program: PathBuf,
    pub env: Vec<(String, String)>,
    /// Where to look for another `swupdate -i` already writing a slot.
    pub proc_root: PathBuf,
}

impl Default for SwupdateInstaller {
    fn default() -> Self {
        Self {
            program: PathBuf::from("swupdate"),
            env: Vec::new(),
            proc_root: PathBuf::from("/proc"),
        }
    }
}

/// Whether another `swupdate ... -i <bundle>` is running, such as an
/// install by hand over SSH. The system swupdate daemon (no `-i`) is idle
/// unless something talks to its socket, so it does not count.
fn another_install_running(proc_root: &Path) -> bool {
    let own = std::process::id().to_string();
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|byte| byte.is_ascii_digit()) || name == own {
            return false;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            return false;
        };
        let mut args = cmdline.split(|byte| *byte == 0);
        let is_swupdate = args
            .next()
            .and_then(|program| program.rsplit(|byte| *byte == b'/').next())
            .is_some_and(|program| program == b"swupdate");
        is_swupdate && args.any(|arg| arg == b"-i")
    })
}

impl Installer for SwupdateInstaller {
    fn install(&self, bundle: &Path, slot: Slot) -> Result<(), UpdateError> {
        if another_install_running(&self.proc_root) {
            warn!("another swupdate install is running; not starting one");
            return Err(UpdateError::InstallBusy);
        }
        let output = Command::new(&self.program)
            .args(["-f", "/etc/swupdate.cfg", "-e"])
            .arg(format!("stable,slot_{}", slot.as_str()))
            .arg("-i")
            .arg(bundle)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                warn!(%error, "could not start swupdate");
                UpdateError::InstallFailed
            })?;
        if output.status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        let last = text.lines().rev().find(|line| !line.trim().is_empty());
        warn!(status = %output.status, last = last.unwrap_or(""), "swupdate failed");
        Err(UpdateError::InstallFailed)
    }

    fn reboot(&self) -> Result<(), UpdateError> {
        info!("restarting to finish the update");
        Command::new("systemctl")
            .arg("reboot")
            .status()
            .ok()
            .filter(std::process::ExitStatus::success)
            .map(|_| ())
            .ok_or(UpdateError::InstallFailed)
    }
}

/// Gets online through the Plan 2 internet worker when there is one,
/// otherwise relies on an existing (wired) route.
#[derive(Debug, Clone)]
pub struct InternetNetwork(pub Option<Internet>);

impl Network for InternetNetwork {
    fn acquire(&self, cancelled: &dyn Fn() -> bool) -> Result<(), UpdateError> {
        let Some(internet) = &self.0 else {
            return crate::internet::reachable()
                .then_some(())
                .ok_or(UpdateError::Offline);
        };
        if matches!(internet.state(), InternetState::Online { .. }) {
            return Ok(());
        }
        internet.send(InternetCommand::Connect);
        let started = Instant::now();
        let mut seen_busy = false;
        loop {
            if cancelled() {
                return Err(UpdateError::Cancelled);
            }
            match internet.state() {
                InternetState::Online { .. } => return Ok(()),
                InternetState::Pausing
                | InternetState::Scanning
                | InternetState::Joining { .. } => seen_busy = true,
                // A Failed left over from an earlier attempt is ignored
                // until this attempt has visibly started, or for 5 s.
                InternetState::Failed(_)
                    if seen_busy || started.elapsed() > Duration::from_secs(5) =>
                {
                    return Err(UpdateError::Offline);
                }
                _ => {}
            }
            if started.elapsed() > ACQUIRE_TIMEOUT {
                return Err(UpdateError::Offline);
            }
            thread::sleep(ACQUIRE_POLL);
        }
    }

    fn release(&self) {
        if let Some(internet) = &self.0 {
            internet.send(InternetCommand::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Duration,
    };

    use sha2::{Digest, Sha256};

    use super::*;

    /// Serves `body` once over HTTP/1.1, optionally announcing a different
    /// length or pausing between chunks.
    fn serve(body: Vec<u8>, announced: usize, chunk_pause: Option<Duration>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok() && line != "\r\n" {
                line.clear();
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {announced}\r\nConnection: close\r\n\r\n"
            );
            for chunk in body.chunks(16 * 1024) {
                if stream.write_all(chunk).is_err() {
                    return;
                }
                if let Some(pause) = chunk_pause {
                    thread::sleep(pause);
                }
            }
        });
        format!("http://{address}/bundle.swu")
    }

    fn asset(url: String, size: u64, sha256: Option<[u8; 32]>) -> Asset {
        Asset {
            name: "bundle.swu".to_owned(),
            size,
            url,
            sha256,
        }
    }

    #[test]
    fn rate_limit_maps_to_busy() {
        assert_eq!(
            map_status(403, UpdateError::GithubUnreachable),
            UpdateError::GithubBusy
        );
        assert_eq!(
            map_status(429, UpdateError::DownloadFailed),
            UpdateError::GithubBusy
        );
        assert_eq!(
            map_status(500, UpdateError::GithubUnreachable),
            UpdateError::GithubUnreachable
        );
        assert_eq!(
            map_status(404, UpdateError::DownloadFailed),
            UpdateError::DownloadFailed
        );
    }

    #[test]
    fn download_checks_size_and_digest() {
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("bundle.swu");
        let body = vec![7_u8; 100_000];
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let source = GithubSource::new("1.0.0").allowing_http();
        let mut seen = 0;
        source
            .download(
                &asset(
                    serve(body.clone(), body.len(), None),
                    body.len() as u64,
                    Some(digest),
                ),
                &dest,
                &mut |done| seen = done,
                &|| false,
            )
            .expect("download");
        assert_eq!(std::fs::read(&dest).expect("file"), body);
        assert_eq!(seen, body.len() as u64);
        assert!(!dir.path().join("bundle.swu.part").exists());
        std::fs::remove_file(&dest).expect("remove");

        let short = source.download(
            &asset(
                serve(body[..50_000].to_vec(), 50_000, None),
                body.len() as u64,
                None,
            ),
            &dest,
            &mut |_| {},
            &|| false,
        );
        assert_eq!(short, Err(UpdateError::DownloadFailed));
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);

        let wrong = source.download(
            &asset(
                serve(body.clone(), body.len(), None),
                body.len() as u64,
                Some([0; 32]),
            ),
            &dest,
            &mut |_| {},
            &|| false,
        );
        assert_eq!(wrong, Err(UpdateError::BadBundle));
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn cancel_mid_download_removes_partial() {
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("bundle.swu");
        let body = vec![1_u8; 1024 * 1024];
        let cancelled = AtomicBool::new(false);
        let result = GithubSource::new("1.0.0").allowing_http().download(
            &asset(
                serve(body.clone(), body.len(), Some(Duration::from_millis(20))),
                body.len() as u64,
                None,
            ),
            &dest,
            &mut |_| cancelled.store(true, Ordering::Relaxed),
            &|| cancelled.load(Ordering::Relaxed),
        );
        assert_eq!(result, Err(UpdateError::Cancelled));
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn stalled_download_fails_instead_of_hanging() {
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("bundle.swu");
        let body = vec![1_u8; 1024 * 1024];
        let started = std::time::Instant::now();
        let result = GithubSource::new("1.0.0")
            .allowing_http()
            .with_read_stall(Duration::from_millis(300))
            .download(
                &asset(
                    serve(body.clone(), body.len(), Some(Duration::from_secs(30))),
                    body.len() as u64,
                    None,
                ),
                &dest,
                &mut |_| {},
                &|| false,
            );
        assert_eq!(result, Err(UpdateError::DownloadFailed));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "gave up promptly"
        );
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn plain_http_download_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let dest = dir.path().join("bundle.swu");
        let body = vec![1_u8; 1000];
        let result = GithubSource::new("1.0.0").download(
            &asset(
                serve(body.clone(), body.len(), None),
                body.len() as u64,
                None,
            ),
            &dest,
            &mut |_| {},
            &|| false,
        );
        assert_eq!(result, Err(UpdateError::DownloadFailed));
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn another_running_install_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let proc_root = dir.path().join("proc");
        let daemon = proc_root.join("456");
        std::fs::create_dir_all(&daemon).expect("daemon");
        std::fs::write(
            daemon.join("cmdline"),
            "/usr/bin/swupdate\0-v\0-f\0/etc/swupdate.cfg\0",
        )
        .expect("cmdline");
        std::fs::create_dir_all(proc_root.join("self")).expect("self");
        std::fs::write(proc_root.join("self/cmdline"), "x\0-i\0").expect("self");
        let log = dir.path().join("ran");
        let script = dir.path().join("fake-swupdate");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", log.display())).expect("script");
        let mut permissions = std::fs::metadata(&script).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        let installer = SwupdateInstaller {
            program: script,
            env: Vec::new(),
            proc_root: proc_root.clone(),
        };
        let bundle = dir.path().join("b.swu");
        std::fs::write(&bundle, "x").expect("bundle");
        assert_eq!(
            installer.install(&bundle, Slot::A),
            Ok(()),
            "an idle daemon is fine"
        );
        assert!(log.exists());
        std::fs::remove_file(&log).expect("reset");

        let manual = proc_root.join("789");
        std::fs::create_dir_all(&manual).expect("manual");
        std::fs::write(
            manual.join("cmdline"),
            "swupdate\0-f\0/etc/swupdate.cfg\0-e\0stable,slot_a\0-i\0/var/manual.swu\0",
        )
        .expect("cmdline");
        assert_eq!(
            installer.install(&bundle, Slot::A),
            Err(UpdateError::InstallBusy)
        );
        assert!(!log.exists(), "swupdate is not started");
    }

    #[test]
    fn swupdate_command_line() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("args");
        let script = dir.path().join("fake-swupdate");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {}\nexit ${{FAKE_EXIT:-0}}\n",
                log.display()
            ),
        )
        .expect("script");
        let mut permissions = std::fs::metadata(&script).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        let bundle = dir.path().join("b.swu");
        let installer = SwupdateInstaller {
            program: script.clone(),
            env: Vec::new(),
            proc_root: dir.path().join("no-proc"),
        };
        installer.install(&bundle, Slot::A).expect("install");
        assert_eq!(
            std::fs::read_to_string(&log).expect("log").trim(),
            format!(
                "-f /etc/swupdate.cfg -e stable,slot_a -i {}",
                bundle.display()
            )
        );
        let failing = SwupdateInstaller {
            program: script,
            env: vec![("FAKE_EXIT".to_owned(), "1".to_owned())],
            proc_root: dir.path().join("no-proc"),
        };
        assert_eq!(
            failing.install(&bundle, Slot::B),
            Err(UpdateError::InstallFailed)
        );
    }
}
