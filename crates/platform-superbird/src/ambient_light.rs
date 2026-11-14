use std::{
    collections::VecDeque,
    fs,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

pub const BRIGHTNESS_PERCENT_RANGE: RangeInclusive<u8> = 1..=100;
pub const DEFAULT_BRIGHTNESS_PERCENT: u8 = 70;

const IIO_BUS_DIR: &str = "/sys/bus/iio/devices";
const BACKLIGHT_CLASS_DIR: &str = "/sys/class/backlight";
const DEFAULT_POLL_INTERVAL_MS: u64 = 200;
const DEFAULT_MEDIAN_WINDOW: usize = 11;
const DEFAULT_RAW_AT_MAX_BRIGHTNESS: u32 = 1_500;
const DEFAULT_MINIMUM_BRIGHTNESS: u32 = 16;
const DEFAULT_DIM_KNEE: u32 = 3;
const DEFAULT_NIGHT_MODE_ON_DARKNESS_PERCENT: u8 = 70;
const DEFAULT_NIGHT_MODE_OFF_DARKNESS_PERCENT: u8 = 50;
const DEFAULT_NIGHT_MODE_ON_DELAY_MS: u64 = 8_000;
const DEFAULT_NIGHT_MODE_OFF_DELAY_MS: u64 = 3_000;
const DEFAULT_INTEGRATION_TIME_MS: u32 = 100;
const DEFAULT_GAIN: u32 = 16;
const BACKLIGHT_TICK: Duration = Duration::from_millis(40);
const BACKLIGHT_STEP_FRACTION: f64 = 0.02;
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(5);
const INITIAL_SAMPLES: usize = 5;
const INITIAL_SAMPLE_MARGIN: Duration = Duration::from_millis(10);
const INITIAL_DISCOVERY_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AmbientLightConfig {
    pub enabled: bool,
    pub automatic_brightness: bool,
    pub brightness_percent: u8,
    pub automatic_night_mode: bool,
    pub poll_interval_ms: u64,
    pub median_window: usize,
    pub raw_at_max_brightness: u32,
    pub minimum_brightness: u32,
    pub dim_knee: u32,
    pub night_mode_on_darkness_percent: u8,
    pub night_mode_off_darkness_percent: u8,
    pub night_mode_on_delay_ms: u64,
    pub night_mode_off_delay_ms: u64,
    pub integration_time_ms: u32,
    pub gain: u32,
}

impl Default for AmbientLightConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            automatic_brightness: true,
            brightness_percent: DEFAULT_BRIGHTNESS_PERCENT,
            automatic_night_mode: true,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            median_window: DEFAULT_MEDIAN_WINDOW,
            raw_at_max_brightness: DEFAULT_RAW_AT_MAX_BRIGHTNESS,
            minimum_brightness: DEFAULT_MINIMUM_BRIGHTNESS,
            dim_knee: DEFAULT_DIM_KNEE,
            night_mode_on_darkness_percent: DEFAULT_NIGHT_MODE_ON_DARKNESS_PERCENT,
            night_mode_off_darkness_percent: DEFAULT_NIGHT_MODE_OFF_DARKNESS_PERCENT,
            night_mode_on_delay_ms: DEFAULT_NIGHT_MODE_ON_DELAY_MS,
            night_mode_off_delay_ms: DEFAULT_NIGHT_MODE_OFF_DELAY_MS,
            integration_time_ms: DEFAULT_INTEGRATION_TIME_MS,
            gain: DEFAULT_GAIN,
        }
    }
}

#[derive(Debug, Error)]
pub enum AmbientLightError {
    #[error("starting the ambient-light monitor thread failed: {0}")]
    Thread(#[source] std::io::Error),
    #[error("no ambient-light sensor appeared")]
    SensorUnavailable,
    #[error("no display backlight appeared")]
    BacklightUnavailable,
    #[error("ambient-light sensor reads failed")]
    SensorRead,
    #[error("writing the display backlight failed: {0}")]
    BacklightWrite(#[source] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialBacklight {
    pub raw: u32,
    pub brightness: u32,
    pub max_brightness: u32,
}

pub fn apply_initial_backlight(
    config: &AmbientLightConfig,
    timeout: Duration,
) -> Result<InitialBacklight, AmbientLightError> {
    let deadline = Instant::now() + timeout;
    let mut sensor = None;
    let mut backlight = None;
    while sensor.is_none() || backlight.is_none() {
        if sensor.is_none() {
            sensor = discover_ambient_light_device();
        }
        if backlight.is_none() {
            backlight = discover_backlight_device();
        }
        if Instant::now() >= deadline {
            break;
        }
        if sensor.is_none() || backlight.is_none() {
            thread::sleep(INITIAL_DISCOVERY_POLL);
        }
    }
    let sensor = sensor.ok_or(AmbientLightError::SensorUnavailable)?;
    let backlight = backlight.ok_or(AmbientLightError::BacklightUnavailable)?;

    configure_ambient_light_device(&sensor, config);
    let interval =
        Duration::from_millis(u64::from(config.integration_time_ms)) + INITIAL_SAMPLE_MARGIN;
    let mut samples = Vec::with_capacity(INITIAL_SAMPLES);
    for _ in 0..INITIAL_SAMPLES {
        thread::sleep(interval);
        if let Some(raw) = read_u32(&sensor.raw_path) {
            samples.push(raw);
        }
    }
    samples.sort_unstable();
    let raw = *samples
        .get(samples.len() / 2)
        .ok_or(AmbientLightError::SensorRead)?;
    let brightness = ambient_to_backlight_target(
        raw,
        backlight.max_brightness,
        config.minimum_brightness,
        config.raw_at_max_brightness,
        config.dim_knee,
    );
    write_backlight(&backlight, brightness).map_err(AmbientLightError::BacklightWrite)?;
    Ok(InitialBacklight {
        raw,
        brightness,
        max_brightness: backlight.max_brightness,
    })
}

pub fn apply_manual_backlight(
    config: &AmbientLightConfig,
    timeout: Duration,
) -> Result<InitialBacklight, AmbientLightError> {
    let deadline = Instant::now() + timeout;
    let backlight = loop {
        if let Some(backlight) = discover_backlight_device() {
            break backlight;
        }
        if Instant::now() >= deadline {
            return Err(AmbientLightError::BacklightUnavailable);
        }
        thread::sleep(INITIAL_DISCOVERY_POLL);
    };
    let brightness = manual_backlight_target(
        config.brightness_percent,
        backlight.max_brightness,
        config.minimum_brightness,
    );
    write_backlight(&backlight, brightness).map_err(AmbientLightError::BacklightWrite)?;
    Ok(InitialBacklight {
        raw: 0,
        brightness,
        max_brightness: backlight.max_brightness,
    })
}

#[derive(Debug, Clone, Default)]
pub struct AmbientLightState {
    night_mode: Arc<AtomicBool>,
}

impl AmbientLightState {
    pub fn night_mode(&self) -> bool {
        self.night_mode.load(Ordering::Acquire)
    }

    fn set_night_mode(&self, enabled: bool) {
        self.night_mode.store(enabled, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
pub struct AmbientLightControl {
    automatic_brightness: Arc<AtomicBool>,
    brightness_percent: Arc<AtomicU8>,
    automatic_night_mode: Arc<AtomicBool>,
    state: AmbientLightState,
    worker: Arc<OnceLock<Thread>>,
}

impl AmbientLightControl {
    fn new(config: &AmbientLightConfig, state: AmbientLightState) -> Self {
        Self {
            automatic_brightness: Arc::new(AtomicBool::new(
                config.enabled && config.automatic_brightness,
            )),
            brightness_percent: Arc::new(AtomicU8::new(clamp_brightness_percent(
                config.brightness_percent,
            ))),
            automatic_night_mode: Arc::new(AtomicBool::new(
                config.enabled && config.automatic_night_mode,
            )),
            state,
            worker: Arc::new(OnceLock::new()),
        }
    }

    fn wake_worker(&self) {
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    pub fn automatic_brightness(&self) -> bool {
        self.automatic_brightness.load(Ordering::Acquire)
    }

    pub fn set_automatic_brightness(&self, enabled: bool) {
        self.automatic_brightness.store(enabled, Ordering::Release);
        self.wake_worker();
    }

    pub fn brightness_percent(&self) -> u8 {
        self.brightness_percent.load(Ordering::Acquire)
    }

    pub fn set_brightness_percent(&self, percent: u8) {
        self.brightness_percent
            .store(clamp_brightness_percent(percent), Ordering::Release);
        self.wake_worker();
    }

    pub fn automatic_night_mode(&self) -> bool {
        self.automatic_night_mode.load(Ordering::Acquire)
    }

    pub fn set_automatic_night_mode(&self, enabled: bool) {
        self.automatic_night_mode.store(enabled, Ordering::Release);
        if !enabled {
            self.state.set_night_mode(false);
        }
    }
}

#[derive(Debug)]
pub struct AmbientLightMonitor {
    state: AmbientLightState,
    control: AmbientLightControl,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AmbientLightMonitor {
    pub fn start(config: AmbientLightConfig) -> Result<Self, AmbientLightError> {
        let state = AmbientLightState::default();
        let control = AmbientLightControl::new(&config, state.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let worker = if config.enabled {
            let worker_state = state.clone();
            let worker_control = control.clone();
            let worker_stop = Arc::clone(&stop);
            let worker = thread::Builder::new()
                .name("ct-ambient-light".to_owned())
                .spawn(move || monitor_loop(config, worker_state, worker_control, worker_stop))
                .map_err(AmbientLightError::Thread)?;
            let _ = control.worker.set(worker.thread().clone());
            Some(worker)
        } else {
            None
        };
        Ok(Self {
            state,
            control,
            stop,
            worker,
        })
    }

    pub fn state(&self) -> AmbientLightState {
        self.state.clone()
    }

    pub fn control(&self) -> AmbientLightControl {
        self.control.clone()
    }
}

impl Drop for AmbientLightMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone)]
struct AmbientLightDevice {
    raw_path: PathBuf,
    integration_time_path: PathBuf,
    gain_path: PathBuf,
}

#[derive(Debug, Clone)]
struct BacklightDevice {
    brightness_path: PathBuf,
    actual_brightness_path: PathBuf,
    max_brightness: u32,
}

fn monitor_loop(
    config: AmbientLightConfig,
    state: AmbientLightState,
    control: AmbientLightControl,
    stop: Arc<AtomicBool>,
) {
    let poll_interval =
        Duration::from_millis(config.poll_interval_ms).clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL);
    let mut sensor = None;
    let mut backlight = None;
    let mut samples = VecDeque::with_capacity(config.median_window);
    let mut ambient = None;
    let mut next_sample = Instant::now();
    let mut current_brightness = None;
    let mut ramp = BacklightRamp::default();
    let mut last_night_mode = state.night_mode();
    let mut pending_night_mode = None;
    let mut brightness_write_failed = false;

    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_sample {
            next_sample = now + poll_interval;
            if sensor.is_none() {
                sensor = discover_ambient_light_device();
                if let Some(device) = &sensor {
                    configure_ambient_light_device(device, &config);
                    info!(path = %device.raw_path.display(), "using ambient-light sensor");
                }
            }
            if let Some(device) = sensor.as_ref() {
                if let Some(raw) = read_u32(&device.raw_path) {
                    samples.push_back(raw);
                    if samples.len() > config.median_window {
                        samples.pop_front();
                    }
                    if let Some(median) = median_of_samples(&samples, config.median_window) {
                        ambient = Some(median);
                    }
                } else {
                    debug!(path = %device.raw_path.display(), "ambient-light read failed; rediscovering sensor");
                    sensor = None;
                    samples.clear();
                }
            }

            if let Some(median) = ambient
                && control.automatic_night_mode()
            {
                let darkness = ambient_darkness_percent(median);
                let transition = night_mode_transition(
                    last_night_mode,
                    &mut pending_night_mode,
                    darkness,
                    now,
                    &config,
                );
                if let Some(next_night_mode) = transition {
                    pending_night_mode = None;
                    last_night_mode = next_night_mode;
                    state.set_night_mode(next_night_mode);
                    info!(
                        raw = median,
                        darkness_percent = darkness,
                        night_mode = next_night_mode,
                        "ambient-light UI mode changed"
                    );
                }
            } else if !control.automatic_night_mode() {
                pending_night_mode = None;
                if last_night_mode {
                    last_night_mode = false;
                    state.set_night_mode(false);
                }
            }
        }

        let mut ramping = false;
        let level = if control.automatic_brightness() {
            ambient.map(BacklightLevel::Ambient)
        } else {
            Some(BacklightLevel::Manual(control.brightness_percent()))
        };
        if let Some(level) = level {
            if backlight.is_none() {
                backlight = discover_backlight_device();
                if let Some(device) = &backlight {
                    info!(
                        path = %device.brightness_path.display(),
                        max_brightness = device.max_brightness,
                        "using display backlight"
                    );
                }
            }

            if let Some(device) = backlight.as_ref() {
                let (target, next) = match level {
                    BacklightLevel::Ambient(median) => {
                        let minimum = config.minimum_brightness.min(device.max_brightness);
                        let target = ambient_to_backlight_target(
                            median,
                            device.max_brightness,
                            config.minimum_brightness,
                            config.raw_at_max_brightness,
                            config.dim_knee,
                        );
                        let current = current_brightness
                            .or_else(|| read_backlight(device))
                            .unwrap_or(device.max_brightness)
                            .min(device.max_brightness);
                        current_brightness = Some(current);
                        (
                            target,
                            ramp.advance(current, target, minimum, device.max_brightness),
                        )
                    }
                    BacklightLevel::Manual(percent) => {
                        ramp = BacklightRamp::default();
                        let target = manual_backlight_target(
                            percent,
                            device.max_brightness,
                            config.minimum_brightness,
                        );
                        (target, target)
                    }
                };
                if current_brightness != Some(next) {
                    match write_backlight(device, next) {
                        Ok(()) => {
                            current_brightness = Some(next);
                            ramping = next != target;
                            if brightness_write_failed {
                                info!("display backlight writes recovered");
                                brightness_write_failed = false;
                            }
                        }
                        Err(error) => {
                            current_brightness = None;
                            if !brightness_write_failed {
                                warn!(
                                    error = %error,
                                    path = %device.brightness_path.display(),
                                    "display backlight write failed"
                                );
                                brightness_write_failed = true;
                            }
                        }
                    }
                }
            }
        }

        let until_sample = next_sample.saturating_duration_since(Instant::now());
        thread::park_timeout(if ramping {
            until_sample.min(BACKLIGHT_TICK)
        } else {
            until_sample
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum BacklightLevel {
    Ambient(u32),
    Manual(u8),
}

fn clamp_brightness_percent(percent: u8) -> u8 {
    percent.clamp(
        *BRIGHTNESS_PERCENT_RANGE.start(),
        *BRIGHTNESS_PERCENT_RANGE.end(),
    )
}

fn manual_backlight_target(percent: u8, max_brightness: u32, minimum_brightness: u32) -> u32 {
    let minimum = minimum_brightness.min(max_brightness);
    let ratio = (f64::from(clamp_brightness_percent(percent)) / 100.0).powi(2);
    minimum + ((f64::from(max_brightness - minimum) * ratio).round() as u32)
}

fn discover_ambient_light_device() -> Option<AmbientLightDevice> {
    let mut devices = fs::read_dir(IIO_BUS_DIR)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("iio:device"))
        })
        .collect::<Vec<_>>();
    devices.sort();

    for device_path in devices {
        let raw_path = [
            device_path.join("in_intensity0_raw"),
            device_path.join("in_illuminance0_input"),
        ]
        .into_iter()
        .find(|path| read_u32(path).is_some());
        let Some(raw_path) = raw_path else {
            continue;
        };
        return Some(AmbientLightDevice {
            integration_time_path: device_path.join("in_intensity0_integration_time"),
            gain_path: device_path.join("in_intensity0_calibscale"),
            raw_path,
        });
    }
    None
}

fn configure_ambient_light_device(device: &AmbientLightDevice, config: &AmbientLightConfig) {
    if device
        .raw_path
        .file_name()
        .is_some_and(|name| name == "in_intensity0_raw")
    {
        let integration_time = format!("{:.3}", f64::from(config.integration_time_ms) / 1_000.0);
        if let Err(error) = fs::write(&device.integration_time_path, integration_time) {
            warn!(
                error = %error,
                path = %device.integration_time_path.display(),
                "failed to configure ambient-light integration time"
            );
        }
        if let Err(error) = fs::write(&device.gain_path, config.gain.to_string()) {
            warn!(
                error = %error,
                path = %device.gain_path.display(),
                "failed to configure ambient-light gain"
            );
        }
    }
}

fn discover_backlight_device() -> Option<BacklightDevice> {
    let mut devices = fs::read_dir(BACKLIGHT_CLASS_DIR)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    devices.sort();

    devices.into_iter().find_map(|device_path| {
        let brightness_path = device_path.join("brightness");
        let actual_brightness_path = device_path.join("actual_brightness");
        let max_brightness = read_u32(&device_path.join("max_brightness"))?;
        (max_brightness > 0 && read_u32(&brightness_path).is_some()).then_some(BacklightDevice {
            brightness_path,
            actual_brightness_path,
            max_brightness,
        })
    })
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_backlight(device: &BacklightDevice) -> Option<u32> {
    read_u32(&device.actual_brightness_path)
        .or_else(|| read_u32(&device.brightness_path))
        .map(|value| value.min(device.max_brightness))
}

fn write_backlight(device: &BacklightDevice, value: u32) -> std::io::Result<()> {
    fs::write(
        &device.brightness_path,
        value.min(device.max_brightness).to_string(),
    )
}

fn median_of_samples(samples: &VecDeque<u32>, window: usize) -> Option<u32> {
    if samples.len() < window || window == 0 {
        return None;
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
}

fn ambient_darkness_percent(raw: u32) -> u8 {
    const DARKEST_LEVEL: u32 = 235;
    const BRIGHTEST_LEVEL: u32 = 50;
    const LEVEL_MAX: u32 = 255;
    const CURVE_MIN_RAW: u32 = 13;
    const CURVE_MAX_RAW: u32 = 1_999;

    let level = if raw < CURVE_MIN_RAW {
        DARKEST_LEVEL
    } else if raw > CURVE_MAX_RAW {
        BRIGHTEST_LEVEL
    } else {
        let curve_input = (f64::from(raw) * -182.88 + 2_219.9).trunc().abs();
        (curve_input.ln() * -19.324 + 297.48)
            .round()
            .clamp(0.0, f64::from(LEVEL_MAX)) as u32
    };
    (100 * level.min(LEVEL_MAX) / LEVEL_MAX) as u8
}

fn night_mode_for_darkness(
    current: bool,
    darkness: u8,
    on_threshold: u8,
    off_threshold: u8,
) -> bool {
    if current {
        darkness > off_threshold
    } else {
        darkness >= on_threshold
    }
}

fn night_mode_transition(
    current: bool,
    pending: &mut Option<(bool, Instant)>,
    darkness: u8,
    now: Instant,
    config: &AmbientLightConfig,
) -> Option<bool> {
    let candidate = night_mode_for_darkness(
        current,
        darkness,
        config.night_mode_on_darkness_percent,
        config.night_mode_off_darkness_percent,
    );
    if candidate == current {
        *pending = None;
        return None;
    }

    let delay = Duration::from_millis(if candidate {
        config.night_mode_on_delay_ms
    } else {
        config.night_mode_off_delay_ms
    });
    match *pending {
        Some((pending_mode, started_at)) if pending_mode == candidate => {
            (now.duration_since(started_at) >= delay).then_some(candidate)
        }
        _ => {
            *pending = Some((candidate, now));
            None
        }
    }
}

fn ambient_to_backlight_target(
    raw: u32,
    max_brightness: u32,
    minimum_brightness: u32,
    raw_at_max_brightness: u32,
    dim_knee: u32,
) -> u32 {
    let minimum = minimum_brightness.min(max_brightness);
    if raw <= dim_knee || raw_at_max_brightness <= dim_knee {
        return minimum;
    }
    let normalized = f64::from(raw - dim_knee) / f64::from(raw_at_max_brightness - dim_knee);
    let ratio = normalized.clamp(0.0, 1.0).sqrt();
    minimum + ((f64::from(max_brightness - minimum) * ratio).round() as u32)
}

#[derive(Debug, Default)]
struct BacklightRamp {
    target: Option<u32>,
    step: i64,
}

impl BacklightRamp {
    fn advance(&mut self, current: u32, target: u32, minimum: u32, maximum: u32) -> u32 {
        let direction = (i64::from(target) - i64::from(current)).signum();
        if self.target != Some(target) || self.step.signum() != direction {
            self.step = ramp_step(current, target);
        }
        self.target = Some(target);
        if current == target {
            return current;
        }
        let next = i64::from(current) + self.step;
        let next = if (self.step > 0 && next >= i64::from(target))
            || (self.step < 0 && next <= i64::from(target))
        {
            i64::from(target)
        } else {
            next
        };
        next.clamp(i64::from(minimum), i64::from(maximum)) as u32
    }
}

fn ramp_step(current: u32, target: u32) -> i64 {
    let difference = i64::from(target) - i64::from(current);
    if difference == 0 {
        return 0;
    }
    let magnitude =
        ((difference.unsigned_abs() as f64 * BACKLIGHT_STEP_FRACTION).round() as i64).max(1);
    magnitude * difference.signum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn darkness_curve_matches_the_nocturne_boundaries() {
        assert_eq!(ambient_darkness_percent(0), 92);
        assert_eq!(ambient_darkness_percent(12), 92);
        assert_eq!(ambient_darkness_percent(13), 78);
        assert_eq!(ambient_darkness_percent(100), 43);
        assert_eq!(ambient_darkness_percent(1_999), 19);
        assert_eq!(ambient_darkness_percent(2_000), 19);
    }

    #[test]
    fn night_mode_thresholds_have_hysteresis() {
        assert!(night_mode_for_darkness(false, 70, 70, 50));
        assert!(!night_mode_for_darkness(false, 69, 70, 50));
        assert!(night_mode_for_darkness(true, 51, 70, 50));
        assert!(!night_mode_for_darkness(true, 50, 70, 50));
    }

    #[test]
    fn automatic_brightness_and_night_mode_are_independent_controls() {
        let state = AmbientLightState::default();
        let control = AmbientLightControl::new(&AmbientLightConfig::default(), state.clone());
        assert!(control.automatic_brightness());
        assert!(control.automatic_night_mode());
        control.set_automatic_brightness(false);
        assert!(!control.automatic_brightness());
        assert!(control.automatic_night_mode());
        control.set_automatic_night_mode(false);
        assert!(!control.automatic_night_mode());
        assert!(!state.night_mode());
    }

    #[test]
    fn manual_brightness_is_clamped_and_follows_a_perceptual_curve() {
        let control = AmbientLightControl::new(
            &AmbientLightConfig {
                brightness_percent: 0,
                ..AmbientLightConfig::default()
            },
            AmbientLightState::default(),
        );
        assert_eq!(control.brightness_percent(), 1);
        control.set_brightness_percent(200);
        assert_eq!(control.brightness_percent(), 100);

        let mut previous = 16;
        for percent in 1..=100 {
            let target = manual_backlight_target(percent, 160, 16);
            assert!((16..=160).contains(&target));
            assert!(target >= previous);
            previous = target;
        }
        assert_eq!(manual_backlight_target(1, 160, 16), 16);
        assert_eq!(manual_backlight_target(50, 160, 16), 52);
        assert_eq!(manual_backlight_target(100, 160, 16), 160);
        assert_eq!(manual_backlight_target(100, 8, 16), 8);
    }

    #[test]
    fn night_mode_requires_a_sustained_cabin_change() {
        let start = Instant::now();
        let config = AmbientLightConfig::default();
        let mut pending = None;
        assert_eq!(
            night_mode_transition(false, &mut pending, 80, start, &config),
            None
        );
        assert_eq!(
            night_mode_transition(
                false,
                &mut pending,
                80,
                start + Duration::from_secs(7),
                &config,
            ),
            None
        );
        assert_eq!(
            night_mode_transition(
                false,
                &mut pending,
                80,
                start + Duration::from_secs(8),
                &config,
            ),
            Some(true)
        );
        assert_eq!(
            night_mode_transition(
                true,
                &mut pending,
                45,
                start + Duration::from_secs(10),
                &config,
            ),
            None
        );
        assert_eq!(
            night_mode_transition(
                true,
                &mut pending,
                45,
                start + Duration::from_secs(13),
                &config,
            ),
            Some(false)
        );
    }

    #[test]
    fn brightness_curve_is_bounded_and_monotonic() {
        let mut previous = 16;
        for raw in 0..=2_000 {
            let target = ambient_to_backlight_target(raw, 160, 16, 1_500, 3);
            assert!((16..=160).contains(&target));
            assert!(target >= previous);
            previous = target;
        }
        assert_eq!(ambient_to_backlight_target(0, 160, 16, 1_500, 3), 16);
        assert_eq!(ambient_to_backlight_target(3, 160, 16, 1_500, 3), 16);
        assert_eq!(ambient_to_backlight_target(1_500, 160, 16, 1_500, 3), 160);
        assert_eq!(
            ambient_to_backlight_target(u32::MAX, 160, 16, 1_500, 3),
            160
        );
    }

    #[test]
    fn median_waits_for_a_full_window_and_rejects_single_spikes() {
        let samples = VecDeque::from([0, 100, 100, 100, 100, 100, 100, 100, 100, 100, 1_000]);
        assert_eq!(median_of_samples(&samples, 11), Some(100));
        assert_eq!(median_of_samples(&samples, 12), None);
    }

    #[test]
    fn brightness_curve_matches_nocturne() {
        assert_eq!(ambient_to_backlight_target(4, 160, 16, 1_500, 3), 20);
        assert_eq!(ambient_to_backlight_target(31, 160, 16, 1_500, 3), 36);
        assert_eq!(ambient_to_backlight_target(100, 160, 16, 1_500, 3), 53);
        assert_eq!(ambient_to_backlight_target(500, 160, 16, 1_500, 3), 99);
        assert_eq!(ambient_to_backlight_target(1_000, 160, 16, 1_500, 3), 134);
        assert_eq!(ambient_to_backlight_target(0, 8, 16, 1_500, 3), 8);
    }

    #[test]
    fn ramp_keeps_a_fixed_step_toward_a_steady_target() {
        let mut ramp = BacklightRamp::default();
        assert_eq!(ramp.advance(16, 160, 16, 160), 19);
        assert_eq!(ramp.advance(19, 160, 16, 160), 22);
        assert_eq!(ramp.advance(159, 160, 16, 160), 160);
        assert_eq!(ramp.advance(160, 160, 16, 160), 160);
        assert_eq!(ramp.advance(160, 16, 16, 160), 157);
        assert_eq!(ramp.advance(157, 16, 16, 160), 154);
        assert_eq!(BacklightRamp::default().advance(0, 160, 16, 160), 16);
        assert_eq!(BacklightRamp::default().advance(5, 16, 16, 160), 16);
    }

    #[test]
    fn ramp_restarts_its_step_when_the_target_moves() {
        let mut ramp = BacklightRamp::default();
        assert_eq!(ramp.advance(16, 160, 16, 160), 19);
        assert_eq!(ramp.advance(19, 30, 16, 160), 20);
        assert_eq!(ramp.advance(20, 30, 16, 160), 21);
    }
}
