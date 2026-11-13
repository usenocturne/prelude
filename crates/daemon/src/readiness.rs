//! Boot readiness for the A/B update health gate.
//!
//! The image only marks a freshly installed slot good after `preluded` has run
//! with its essential workers alive for a continuous stability window. The
//! marker lives on tmpfs, so every boot starts unready.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

pub const READY_MARKER: &str = "/run/prelude/ready";
pub const STABILITY_WINDOW: Duration = Duration::from_secs(45);
/// A worker whose heartbeat is older than this counts as not running. Must stay
/// well above the host UI's longest loop wait (500 ms input retry).
pub const HEARTBEAT_STALE_AFTER: Duration = Duration::from_secs(5);

/// Writes the readiness marker once essentials have been continuously healthy
/// for [`STABILITY_WINDOW`].
#[derive(Debug)]
pub struct Readiness {
    healthy_since: Option<Instant>,
    path: PathBuf,
    written: bool,
}

impl Readiness {
    pub fn new(path: PathBuf) -> Self {
        Self {
            healthy_since: None,
            path,
            written: false,
        }
    }

    /// Returns `true` only on the tick that wrote the marker.
    pub fn tick(&mut self, now: Instant, essentials_ok: bool) -> std::io::Result<bool> {
        if self.written {
            return Ok(false);
        }
        if !essentials_ok {
            self.healthy_since = None;
            return Ok(false);
        }
        let since = *self.healthy_since.get_or_insert(now);
        if now.saturating_duration_since(since) < STABILITY_WINDOW {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, format!("{}\n", env!("CARGO_PKG_VERSION")))?;
        self.written = true;
        Ok(true)
    }
}

/// Liveness signal a worker thread updates every loop iteration.
#[derive(Debug)]
pub struct Heartbeat {
    base: Instant,
    last_millis: AtomicU64,
}

impl Heartbeat {
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub fn new_at(now: Instant) -> Self {
        Self {
            base: now,
            last_millis: AtomicU64::new(0),
        }
    }

    pub fn beat(&self) {
        self.beat_at(Instant::now());
    }

    pub fn beat_at(&self, now: Instant) {
        let millis =
            u64::try_from(now.saturating_duration_since(self.base).as_millis()).unwrap_or(u64::MAX);
        self.last_millis.store(millis, Ordering::Release);
    }

    pub fn age_at(&self, now: Instant) -> Duration {
        let last = self.base + Duration::from_millis(self.last_millis.load(Ordering::Acquire));
        now.saturating_duration_since(last)
    }

    pub fn is_fresh(&self) -> bool {
        self.is_fresh_at(Instant::now())
    }

    pub fn is_fresh_at(&self, now: Instant) -> bool {
        self.age_at(now) < HEARTBEAT_STALE_AFTER
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn readiness(directory: &tempfile::TempDir) -> (Readiness, std::path::PathBuf) {
        let path = directory.path().join("run/prelude/ready");
        (Readiness::new(path.clone()), path)
    }

    #[test]
    fn not_ready_before_window() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (mut readiness, path) = readiness(&directory);
        let start = Instant::now();
        assert!(!readiness.tick(start, true).expect("tick"));
        assert!(
            !readiness
                .tick(start + Duration::from_secs(44), true)
                .expect("tick")
        );
        assert!(!path.exists());
    }

    #[test]
    fn ready_after_window() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (mut readiness, path) = readiness(&directory);
        let start = Instant::now();
        readiness.tick(start, true).expect("tick");
        assert!(
            readiness
                .tick(start + STABILITY_WINDOW, true)
                .expect("tick")
        );
        assert!(path.is_file());
    }

    #[test]
    fn writes_once() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (mut readiness, _) = readiness(&directory);
        let start = Instant::now();
        readiness.tick(start, true).expect("tick");
        assert!(
            readiness
                .tick(start + STABILITY_WINDOW, true)
                .expect("tick")
        );
        assert!(
            !readiness
                .tick(start + STABILITY_WINDOW * 2, true)
                .expect("tick")
        );
    }

    #[test]
    fn creates_parent_dir() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (mut readiness, path) = readiness(&directory);
        let start = Instant::now();
        readiness.tick(start, true).expect("tick");
        readiness
            .tick(start + STABILITY_WINDOW, true)
            .expect("tick");
        assert!(path.parent().expect("parent").is_dir());
    }

    #[test]
    fn window_resets_when_essentials_fail() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (mut readiness, path) = readiness(&directory);
        let start = Instant::now();
        readiness.tick(start, true).expect("tick");
        readiness
            .tick(start + Duration::from_secs(40), true)
            .expect("tick");
        readiness
            .tick(start + Duration::from_secs(41), false)
            .expect("tick");
        readiness
            .tick(start + Duration::from_secs(42), true)
            .expect("tick");
        assert!(
            !readiness
                .tick(start + Duration::from_secs(80), true)
                .expect("tick")
        );
        assert!(!path.exists());
        assert!(
            readiness
                .tick(start + Duration::from_secs(87), true)
                .expect("tick")
        );
        assert!(path.is_file());
    }

    #[test]
    fn heartbeat_fresh_while_beating() {
        let start = Instant::now();
        let heartbeat = Heartbeat::new_at(start);
        heartbeat.beat_at(start + Duration::from_secs(10));
        assert_eq!(
            heartbeat.age_at(start + Duration::from_secs(11)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn heartbeat_stale_after_worker_stops() {
        let start = Instant::now();
        let heartbeat = Heartbeat::new_at(start);
        heartbeat.beat_at(start + Duration::from_secs(1));
        assert!(heartbeat.age_at(start + Duration::from_secs(7)) > HEARTBEAT_STALE_AFTER);
        assert!(!heartbeat.is_fresh_at(start + Duration::from_secs(7)));
        assert!(heartbeat.is_fresh_at(start + Duration::from_secs(2)));
    }
}
