//! Checking for and installing Prelude updates from GitHub releases.
//!
//! The Updates page sends commands to one worker thread and polls its state,
//! in the same pattern as [`crate::internet`]. Release selection, slot guards
//! and the bundle manifest check are pure functions; GitHub, `swupdate` and
//! the reboot sit behind traits so the worker can be tested with fakes.

pub mod release;
pub mod system;

pub use release::{Asset, Candidate, Flavour, Relation, Release};

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
            Self::InstallFailed => "Install failed. Nothing was changed.",
            Self::RestartPending => "Restart to finish the last update",
            Self::NotHealthy => "Updates need a healthy start first. Try again in a minute.",
            Self::SlotUnknown => "This system's slot could not be read",
            Self::Cancelled => "",
        }
    }
}
