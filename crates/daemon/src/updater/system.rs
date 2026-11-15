//! What the Car Thing itself says about updating: running slot, image
//! flavour, health and pending-install markers, free space, and the version
//! inside a downloaded bundle.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use super::{Flavour, UpdateError};

const PENDING: &str = "pending-update";
const MANIFEST: &str = "sw-description";
const MANIFEST_LIMIT: u64 = 64 * 1024;
const NEWC_HEADER: usize = 110;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

/// `superbird.slot=a|b` from the kernel command line.
pub fn running_slot(cmdline: &str) -> Option<Slot> {
    cmdline
        .split_whitespace()
        .find_map(|word| word.strip_prefix("superbird.slot="))
        .and_then(|slot| match slot {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        })
}

/// What a pending-update marker means for this boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    None,
    /// Installed into the other slot; a restart switches to it.
    NeedsRestart,
    /// Already running the new slot; the marker is cleared once this boot
    /// is marked good.
    OnTrial,
}

/// Reads the marker against the running slot. A marker whose slot cannot
/// be matched counts as needing a restart, which blocks further installs.
pub fn pending_update(state_dir: &Path, cmdline: &Path) -> Pending {
    let Ok(marker) = fs::read_to_string(state_dir.join(PENDING)) else {
        return Pending::None;
    };
    let marked = marker.lines().find_map(|line| line.strip_prefix("slot="));
    let running = fs::read_to_string(cmdline)
        .ok()
        .as_deref()
        .and_then(running_slot);
    match (marked, running) {
        (Some(marked), Some(running)) if marked.trim() == running.as_str() => Pending::OnTrial,
        _ => Pending::NeedsRestart,
    }
}

/// Where the updater looks; tests point these at a temporary directory.
#[derive(Debug, Clone)]
pub struct SystemPaths {
    pub etc_prelude: PathBuf,
    pub cmdline: PathBuf,
    pub ready: PathBuf,
    pub state_dir: PathBuf,
    pub free_bytes: fn(&Path) -> io::Result<u64>,
    /// The slot U-Boot will boot next (`slot_active`), if it can be read.
    pub active_slot: fn() -> Option<Slot>,
}

impl SystemPaths {
    pub fn real(state_dir: PathBuf) -> Self {
        Self {
            etc_prelude: PathBuf::from("/etc/prelude"),
            cmdline: PathBuf::from("/proc/cmdline"),
            ready: PathBuf::from(crate::readiness::READY_MARKER),
            state_dir,
            free_bytes,
            active_slot: uboot_active_slot,
        }
    }

    /// Whether this image was built without an OTA key.
    pub fn ota_disabled(&self) -> bool {
        self.etc_prelude.join("ota-disabled").exists()
    }

    pub fn flavour(&self) -> Option<Flavour> {
        Flavour::parse(&fs::read_to_string(self.etc_prelude.join("flavour")).ok()?)
    }
}

/// Whether an install may start; on success, the slot to install into and
/// the bundle flavour. Checked in this order so the most useful message wins.
pub fn install_guard(paths: &SystemPaths) -> Result<(Slot, Flavour), UpdateError> {
    if paths.ota_disabled() {
        return Err(UpdateError::Unavailable);
    }
    let flavour = paths.flavour().ok_or(UpdateError::Unavailable)?;
    let running = fs::read_to_string(&paths.cmdline)
        .ok()
        .as_deref()
        .and_then(running_slot)
        .ok_or(UpdateError::SlotUnknown)?;
    match pending_update(&paths.state_dir, &paths.cmdline) {
        Pending::NeedsRestart => return Err(UpdateError::RestartPending),
        Pending::OnTrial => return Err(UpdateError::NotHealthy),
        Pending::None => {}
    }
    if !paths.ready.exists() {
        return Err(UpdateError::NotHealthy);
    }
    Ok((running.other(), flavour))
}

/// `fw_printenv -n slot_active`.
fn uboot_active_slot() -> Option<Slot> {
    let output = std::process::Command::new("fw_printenv")
        .args(["-n", "slot_active"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "a" => Some(Slot::A),
        "b" => Some(Slot::B),
        _ => None,
    }
}

/// This boot's kernel boot id.
pub fn current_boot() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// What [`reconcile_pending`] did with a marker an earlier run left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    Kept,
    /// The install never switched slots (the daemon stopped mid-install).
    Removed,
    /// The device restarted into the update and U-Boot fell back.
    FellBack {
        version: String,
    },
}

/// Settles a pending marker when the updater starts. A marker for the
/// running slot is a trial and stays; so does one whose install finished
/// this boot (U-Boot already points at it, waiting for the restart).
/// Otherwise the marker is stale: if U-Boot still points at its slot, the
/// device restarted into it and fell back.
pub fn reconcile_pending(
    state_dir: &Path,
    cmdline: &Path,
    boot: Option<&str>,
    active: Option<Slot>,
) -> Reconciled {
    let Ok(marker) = fs::read_to_string(state_dir.join(PENDING)) else {
        return Reconciled::Kept;
    };
    let field = |key: &str| {
        marker
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
    };
    let marked = match field("slot=") {
        Some("a") => Slot::A,
        Some("b") => Slot::B,
        _ => return Reconciled::Kept,
    };
    let running = fs::read_to_string(cmdline)
        .ok()
        .as_deref()
        .and_then(running_slot);
    let (Some(running), Some(active)) = (running, active) else {
        return Reconciled::Kept;
    };
    if marked == running {
        return Reconciled::Kept;
    }
    let this_boot = boot.is_some() && field("boot=") == boot;
    if active == marked {
        if this_boot {
            return Reconciled::Kept;
        }
        clear_pending(state_dir);
        return Reconciled::FellBack {
            version: field("version=").unwrap_or_default().to_owned(),
        };
    }
    clear_pending(state_dir);
    Reconciled::Removed
}

/// Records the install about to start (Plan 1 contract): written to a
/// temporary file, synced, renamed, then the directory synced. The boot id
/// lets a later start tell "installed, not yet restarted" from "restarted
/// and fell back".
pub fn write_pending(state_dir: &Path, slot: Slot, version: &str) -> io::Result<()> {
    let temporary = state_dir.join(format!("{PENDING}.tmp"));
    let mut file = File::create(&temporary)?;
    write!(file, "slot={}\nversion={version}\n", slot.as_str())?;
    if let Some(boot) = current_boot() {
        writeln!(file, "boot={boot}")?;
    }
    file.sync_all()?;
    fs::rename(&temporary, state_dir.join(PENDING))?;
    File::open(state_dir)?.sync_all()
}

pub fn clear_pending(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join(PENDING));
}

pub fn pending_exists(state_dir: &Path) -> bool {
    state_dir.join(PENDING).exists()
}

/// The `version` in the bundle's `sw-description` (its first newc cpio
/// entry), without any `+build` suffix.
pub fn bundle_version(bundle: &Path) -> Result<String, UpdateError> {
    let mut head = Vec::new();
    File::open(bundle)
        .and_then(|file| {
            file.take(MANIFEST_LIMIT + NEWC_HEADER as u64 + 512)
                .read_to_end(&mut head)
        })
        .map_err(|_| UpdateError::BadBundle)?;
    let manifest = first_newc_entry(&head, MANIFEST).ok_or(UpdateError::BadBundle)?;
    let text = std::str::from_utf8(manifest).map_err(|_| UpdateError::BadBundle)?;
    manifest_version(text)
        .map(|version| version.split('+').next().unwrap_or(version).to_owned())
        .ok_or(UpdateError::BadBundle)
}

fn first_newc_entry<'a>(archive: &'a [u8], expected: &str) -> Option<&'a [u8]> {
    let header = archive.get(..NEWC_HEADER)?;
    // 070701 is plain newc; 070702 (used by SWUpdate) adds a checksum field
    // that sits in the same header slot, so both parse the same way.
    if !(header.starts_with(b"070701") || header.starts_with(b"070702")) {
        return None;
    }
    // After the 6-byte magic: 13 fields of 8 hex digits. filesize is the
    // 7th field, namesize the 12th.
    let field = |index: usize| {
        let start = 6 + index * 8;
        std::str::from_utf8(&header[start..start + 8])
            .ok()
            .and_then(|digits| usize::from_str_radix(digits, 16).ok())
    };
    let file_size = field(6)?;
    let name_size = field(11)?;
    if file_size as u64 > MANIFEST_LIMIT {
        return None;
    }
    let name = archive.get(NEWC_HEADER..NEWC_HEADER + name_size)?;
    if name.strip_suffix(&[0]) != Some(expected.as_bytes()) {
        return None;
    }
    let data_start = (NEWC_HEADER + name_size).next_multiple_of(4);
    archive.get(data_start..data_start + file_size)
}

fn manifest_version(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("version")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start().strip_prefix('"')?;
        let (version, _) = rest.split_once('"')?;
        (!version.is_empty()).then_some(version)
    })
}

/// `VERSION_ID` from `/etc/os-release`, or the daemon's own version.
pub fn installed_version() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|contents| os_release_version(&contents))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

pub fn os_release_version(contents: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("VERSION_ID="))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
}

/// Bytes available to the daemon at `path`.
// statvfs field types differ between 32- and 64-bit targets.
#[allow(clippy::useless_conversion)]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let stats = nix::sys::statvfs::statvfs(path).map_err(io::Error::from)?;
    Ok(u64::from(stats.blocks_available()) * u64::from(stats.fragment_size()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use super::*;

    pub(crate) fn newc(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn pad(buffer: &mut Vec<u8>) {
            while buffer.len() % 4 != 0 {
                buffer.push(0);
            }
        }
        let mut archive = Vec::new();
        for (index, (name, data)) in entries.iter().enumerate() {
            let name_size = name.len() + 1;
            archive.extend_from_slice(b"070701");
            for field in [
                index as u32 + 1, // ino
                0o100_644,        // mode
                0,
                0,
                1,
                0,
                data.len() as u32, // filesize
                0,
                0,
                0,
                0,
                name_size as u32, // namesize
                0,
            ] {
                archive.extend_from_slice(format!("{field:08x}").as_bytes());
            }
            archive.extend_from_slice(name.as_bytes());
            archive.push(0);
            pad(&mut archive);
            archive.extend_from_slice(data);
            pad(&mut archive);
        }
        archive
    }

    fn paths(dir: &Path) -> SystemPaths {
        let etc = dir.join("etc");
        fs::create_dir_all(&etc).expect("etc");
        fs::create_dir_all(dir.join("state")).expect("state");
        fs::write(etc.join("flavour"), "prod\n").expect("flavour");
        fs::write(
            dir.join("cmdline"),
            "console=ttyAML0 superbird.slot=b quiet\n",
        )
        .expect("cmdline");
        fs::write(dir.join("ready"), "").expect("ready");
        SystemPaths {
            etc_prelude: etc,
            cmdline: dir.join("cmdline"),
            ready: dir.join("ready"),
            state_dir: dir.join("state"),
            free_bytes: |_| Ok(u64::MAX),
            active_slot: || None,
        }
    }

    #[test]
    fn os_release_version_reads_version_id() {
        assert_eq!(
            os_release_version(
                "NAME=Prelude
VERSION_ID=\"1.0.12-test\"
"
            ),
            Some("1.0.12-test".to_owned())
        );
        assert_eq!(
            os_release_version(
                "NAME=Prelude
"
            ),
            None
        );
    }

    #[test]
    fn running_slot_reads_cmdline() {
        assert_eq!(running_slot("a superbird.slot=b quiet"), Some(Slot::B));
        assert_eq!(running_slot("superbird.slot=a"), Some(Slot::A));
        assert_eq!(running_slot("quiet"), None);
        assert_eq!(running_slot("superbird.slot=c"), None);
        assert_eq!(running_slot("superbird.slot=ab"), None);
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.as_str(), "b");
    }

    #[test]
    fn guard_happy_path_targets_the_other_slot() {
        let dir = tempfile::tempdir().expect("dir");
        assert_eq!(
            install_guard(&paths(dir.path())),
            Ok((Slot::A, Flavour::Prod))
        );
    }

    #[test]
    fn guard_ota_disabled_is_unavailable() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        fs::write(paths.etc_prelude.join("ota-disabled"), "").expect("marker");
        fs::write(paths.state_dir.join("pending-update"), "slot=a\n").expect("pending");
        assert_eq!(install_guard(&paths), Err(UpdateError::Unavailable));
    }

    #[test]
    fn guard_bad_flavour_is_unavailable() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        fs::write(paths.etc_prelude.join("flavour"), "weird").expect("flavour");
        assert_eq!(install_guard(&paths), Err(UpdateError::Unavailable));
        fs::remove_file(paths.etc_prelude.join("flavour")).expect("remove");
        assert_eq!(install_guard(&paths), Err(UpdateError::Unavailable));
    }

    #[test]
    fn guard_missing_slot_is_slot_unknown() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        fs::write(&paths.cmdline, "quiet").expect("cmdline");
        assert_eq!(install_guard(&paths), Err(UpdateError::SlotUnknown));
    }

    #[test]
    fn guard_pending_marker_is_restart_pending() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        write_pending(&paths.state_dir, Slot::A, "1.0.6").expect("pending");
        fs::remove_file(&paths.ready).expect("not ready either");
        assert_eq!(install_guard(&paths), Err(UpdateError::RestartPending));
    }

    #[test]
    fn pending_update_compares_marker_slot_with_running_slot() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        let pending = || pending_update(&paths.state_dir, &paths.cmdline);
        assert_eq!(pending(), Pending::None);
        write_pending(&paths.state_dir, Slot::A, "1.0.6").expect("pending");
        assert_eq!(pending(), Pending::NeedsRestart);
        write_pending(&paths.state_dir, Slot::B, "1.0.6").expect("pending");
        assert_eq!(pending(), Pending::OnTrial, "running the new slot already");
        fs::remove_file(&paths.cmdline).expect("cmdline");
        assert_eq!(
            pending(),
            Pending::NeedsRestart,
            "unknown slot stays cautious"
        );
    }

    #[test]
    fn guard_trial_boot_is_not_healthy() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        write_pending(&paths.state_dir, Slot::B, "1.0.6").expect("pending");
        assert_eq!(install_guard(&paths), Err(UpdateError::NotHealthy));
    }

    fn marker(state_dir: &Path, slot: &str, boot: &str) {
        fs::write(
            state_dir.join("pending-update"),
            format!("slot={slot}\nversion=1.0.6\nboot={boot}\n"),
        )
        .expect("marker");
    }

    #[test]
    fn write_pending_records_the_boot() {
        let dir = tempfile::tempdir().expect("dir");
        write_pending(dir.path(), Slot::A, "1.0.6").expect("pending");
        let text = fs::read_to_string(dir.path().join("pending-update")).expect("read");
        assert!(text.starts_with("slot=a\nversion=1.0.6\n"));
        if let Some(boot) = current_boot() {
            assert!(text.contains(&format!("boot={boot}\n")));
        }
    }

    #[test]
    fn reconcile_sorts_out_markers_left_by_an_earlier_run() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path()); // running slot b
        let reconcile =
            |active| reconcile_pending(&paths.state_dir, &paths.cmdline, Some("now"), active);

        assert_eq!(reconcile(Some(Slot::A)), Reconciled::Kept, "no marker");

        marker(&paths.state_dir, "a", "now");
        assert_eq!(
            reconcile(Some(Slot::A)),
            Reconciled::Kept,
            "installed, not yet restarted"
        );
        assert!(pending_exists(&paths.state_dir));
        assert_eq!(reconcile(None), Reconciled::Kept, "U-Boot state unknown");

        assert_eq!(
            reconcile(Some(Slot::B)),
            Reconciled::Removed,
            "the daemon stopped mid-install, before the switch"
        );
        assert!(!pending_exists(&paths.state_dir));

        marker(&paths.state_dir, "a", "earlier");
        assert_eq!(
            reconcile(Some(Slot::A)),
            Reconciled::FellBack {
                version: "1.0.6".to_owned()
            },
            "restarted into the update, then fell back"
        );
        assert!(!pending_exists(&paths.state_dir));

        marker(&paths.state_dir, "b", "earlier");
        assert_eq!(reconcile(Some(Slot::B)), Reconciled::Kept, "on its trial");
    }

    #[test]
    fn guard_not_ready_is_not_healthy() {
        let dir = tempfile::tempdir().expect("dir");
        let paths = paths(dir.path());
        fs::remove_file(&paths.ready).expect("remove");
        assert_eq!(install_guard(&paths), Err(UpdateError::NotHealthy));
    }

    #[test]
    fn pending_marker_round_trip() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(!pending_exists(dir.path()));
        write_pending(dir.path(), Slot::A, "1.0.6").expect("write");
        assert!(
            fs::read_to_string(dir.path().join("pending-update"))
                .expect("read")
                .starts_with("slot=a\nversion=1.0.6\n")
        );
        assert!(!dir.path().join("pending-update.tmp").exists());
        assert!(pending_exists(dir.path()));
        clear_pending(dir.path());
        assert!(!pending_exists(dir.path()));
    }

    #[test]
    fn bundle_version_strips_build_metadata() {
        let dir = tempfile::tempdir().expect("dir");
        let bundle = dir.path().join("b.swu");
        let manifest = b"software =\n{\n    version = \"1.0.11-test+20261114182039\";\n    description = \"x\";\n};\n";
        fs::write(
            &bundle,
            newc(&[
                ("sw-description", manifest),
                ("sw-description.sig", b"sig"),
                ("boot.vfat", b"boot"),
            ]),
        )
        .expect("bundle");
        assert_eq!(bundle_version(&bundle), Ok("1.0.11-test".to_owned()));
    }

    #[test]
    fn bundle_version_reads_crc_format_archives() {
        // SWUpdate bundles use the CRC newc variant (magic 070702).
        let dir = tempfile::tempdir().expect("dir");
        let bundle = dir.path().join("b.swu");
        let mut archive = newc(&[(
            "sw-description",
            b"software =
{
    version = \"1.0.21-test+20261114215338\";
};
",
        )]);
        archive[..6].copy_from_slice(b"070702");
        fs::write(&bundle, archive).expect("bundle");
        assert_eq!(bundle_version(&bundle), Ok("1.0.21-test".to_owned()));
    }

    #[test]
    fn bundle_whose_first_entry_is_not_sw_description_is_rejected() {
        let dir = tempfile::tempdir().expect("dir");
        let bundle = dir.path().join("b.swu");
        fs::write(&bundle, newc(&[("boot.vfat", b"version = \"1.0\";")])).expect("bundle");
        assert_eq!(bundle_version(&bundle), Err(UpdateError::BadBundle));
        fs::write(&bundle, b"070701000000").expect("truncated");
        assert_eq!(bundle_version(&bundle), Err(UpdateError::BadBundle));
        fs::write(&bundle, newc(&[("sw-description", b"software = {};")])).expect("no version");
        assert_eq!(bundle_version(&bundle), Err(UpdateError::BadBundle));
    }

    #[test]
    fn free_bytes_reports_something_for_a_real_directory() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(free_bytes(dir.path()).expect("statvfs") > 0);
    }
}
