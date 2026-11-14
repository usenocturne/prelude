use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, OpenOptions},
    io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use evdev::{KeyCode, raw_stream::RawDevice};
use nix::poll::{PollFd, PollFlags, poll};
use thiserror::Error;

use crate::{
    DeviceRole, InputDeviceProfile, InputParser, InputParserConfig, InputParserError,
    MonotonicMicros, PlatformInputEvent, RawInputEvent, ResetReason,
};

const DEFAULT_INPUT_DIRECTORY: &str = "/dev/input";
const DEFAULT_GPIO_NAME: &str = "gpio-keys-polled";
const DEFAULT_ROTARY_NAME: &str = "rotary";
const DEFAULT_TOUCH_NAME: &str = "tlsc6x";
const MAXIMUM_DEVICE_NAME_BYTES: usize = 128;
const MAXIMUM_PATH_BYTES: usize = 4096;
const MAXIMUM_CANDIDATES_LIMIT: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceSelector {
    pub exact_kernel_name: String,
    pub explicit_path: Option<PathBuf>,
}

impl std::fmt::Debug for DeviceSelector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceSelector")
            .field("exact_kernel_name", &self.exact_kernel_name)
            .field(
                "explicit_path",
                &self.explicit_path.as_ref().map(|_| "[configured]"),
            )
            .finish()
    }
}

impl DeviceSelector {
    pub fn named(exact_kernel_name: impl Into<String>) -> Self {
        Self {
            exact_kernel_name: exact_kernel_name.into(),
            explicit_path: None,
        }
    }

    pub fn at_path(exact_kernel_name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            exact_kernel_name: exact_kernel_name.into(),
            explicit_path: Some(path.into()),
        }
    }

    fn validate(&self) -> Result<(), LinuxInputError> {
        if self.exact_kernel_name.is_empty()
            || self.exact_kernel_name.len() > MAXIMUM_DEVICE_NAME_BYTES
            || self.exact_kernel_name.as_bytes().contains(&0)
            || self.exact_kernel_name.chars().any(char::is_control)
        {
            return Err(LinuxInputError::InvalidConfig(
                "kernel device name is empty, too long, or contains control data",
            ));
        }
        if let Some(path) = &self.explicit_path {
            validate_path(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabPolicy {
    NonExclusive,
    Exclusive,
}

#[derive(Clone)]
pub struct LinuxInputConfig {
    pub input_directory: PathBuf,
    pub gpio: DeviceSelector,
    pub rotary: DeviceSelector,
    pub touch: DeviceSelector,
    pub grab_policy: GrabPolicy,
    pub parser: InputParserConfig,
    pub poll_interval: Duration,
    pub maximum_candidates: usize,
}

impl std::fmt::Debug for LinuxInputConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxInputConfig")
            .field("input_directory", &"[configured]")
            .field("gpio", &self.gpio)
            .field("rotary", &self.rotary)
            .field("touch", &self.touch)
            .field("grab_policy", &self.grab_policy)
            .field("parser", &self.parser)
            .field("poll_interval", &self.poll_interval)
            .field("maximum_candidates", &self.maximum_candidates)
            .finish()
    }
}

impl Default for LinuxInputConfig {
    fn default() -> Self {
        Self {
            input_directory: PathBuf::from(DEFAULT_INPUT_DIRECTORY),
            gpio: DeviceSelector::named(DEFAULT_GPIO_NAME),
            rotary: DeviceSelector::named(DEFAULT_ROTARY_NAME),
            touch: DeviceSelector::named(DEFAULT_TOUCH_NAME),
            grab_policy: GrabPolicy::NonExclusive,
            parser: InputParserConfig::default(),
            poll_interval: Duration::from_millis(20),
            maximum_candidates: 64,
        }
    }
}

impl LinuxInputConfig {
    fn validate(self) -> Result<Self, LinuxInputError> {
        validate_path(&self.input_directory)?;
        self.gpio.validate()?;
        self.rotary.validate()?;
        self.touch.validate()?;
        if self.gpio.exact_kernel_name == self.rotary.exact_kernel_name
            || self.gpio.exact_kernel_name == self.touch.exact_kernel_name
            || self.rotary.exact_kernel_name == self.touch.exact_kernel_name
        {
            return Err(LinuxInputError::InvalidConfig(
                "device role kernel names must be distinct",
            ));
        }
        if !(Duration::from_millis(1)..=Duration::from_secs(1)).contains(&self.poll_interval) {
            return Err(LinuxInputError::InvalidConfig(
                "input poll interval must be within 1ms..=1s",
            ));
        }
        if self.maximum_candidates == 0 || self.maximum_candidates > MAXIMUM_CANDIDATES_LIMIT {
            return Err(LinuxInputError::InvalidConfig(
                "maximum input candidates must be within 1..=256",
            ));
        }
        InputParser::new(self.parser)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxInputTerminal {
    Cancelled,
    DeviceUnplugged(DeviceRole),
    DeviceReadFailed(DeviceRole),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxInputBatch {
    pub events: Vec<PlatformInputEvent>,
    pub terminal: Option<LinuxInputTerminal>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TouchWaitWindow {
    pub raw_events: u64,
    pub total_micros: u64,
    pub maximum_micros: u64,
}

impl TouchWaitWindow {
    fn record(&mut self, waited: Duration) {
        let micros = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
        self.raw_events = self.raw_events.saturating_add(1);
        self.total_micros = self.total_micros.saturating_add(micros);
        self.maximum_micros = self.maximum_micros.max(micros);
    }
}

#[derive(Debug, Error)]
pub enum LinuxInputError {
    #[error("invalid Linux input configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("could not enumerate Linux input devices: {0}")]
    Enumeration(io::Error),
    #[error("could not access the configured {role:?} input device: {source}")]
    DeviceAccess {
        role: DeviceRole,
        #[source]
        source: io::Error,
    },
    #[error("no exact kernel-name/capability match for {0:?}")]
    DeviceMissing(DeviceRole),
    #[error("more than one exact kernel-name/capability match for {0:?}")]
    DeviceAmbiguous(DeviceRole),
    #[error("the exact-name {0:?} device is missing required capabilities or axis ranges")]
    CapabilityMismatch(DeviceRole),
    #[error("too many Linux input candidates")]
    TooManyCandidates,
    #[error("exclusive input grab failed for {role:?}: {source}")]
    Grab {
        role: DeviceRole,
        #[source]
        source: io::Error,
    },
    #[error("Linux input poll failed: {0}")]
    Poll(#[source] nix::errno::Errno),
    #[error(transparent)]
    Parser(#[from] InputParserError),
    #[error("Linux input backend is terminal: {0:?}")]
    Terminal(LinuxInputTerminal),
}

#[derive(Debug)]
struct ReceiptClock {
    origin: Instant,
    last: MonotonicMicros,
}

impl ReceiptClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            last: MonotonicMicros(0),
        }
    }

    fn now(&mut self) -> MonotonicMicros {
        let elapsed = u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.last = MonotonicMicros(elapsed.max(self.last.0));
        self.last
    }
}

struct RoleDevice {
    role: DeviceRole,
    device: RawDevice,
}

pub struct LinuxGpioKeyReader {
    device: RawDevice,
}

impl std::fmt::Debug for LinuxGpioKeyReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxGpioKeyReader")
            .field("device", &"[redacted evdev identity]")
            .finish()
    }
}

impl LinuxGpioKeyReader {
    pub fn discover(config: LinuxInputConfig) -> Result<Self, LinuxInputError> {
        let config = config.validate()?;
        let device = discover_role(&config, DeviceRole::GpioKeys, &config.gpio)?;
        Ok(Self { device })
    }

    pub fn is_pressed(&self, key: crate::GpioKey) -> Result<bool, LinuxInputError> {
        self.device
            .get_key_state()
            .map(|pressed| pressed.contains(KeyCode::new(key.linux_code())))
            .map_err(|source| LinuxInputError::DeviceAccess {
                role: DeviceRole::GpioKeys,
                source,
            })
    }
}

impl std::fmt::Debug for RoleDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoleDevice")
            .field("role", &self.role)
            .field("device", &"[redacted evdev identity]")
            .finish()
    }
}

#[derive(Debug)]
pub struct LinuxInputBackend {
    config: LinuxInputConfig,
    devices: [RoleDevice; 3],
    parser: InputParser,
    clock: ReceiptClock,
    terminal: Option<LinuxInputTerminal>,
    touch_wait: TouchWaitWindow,
}

impl LinuxInputBackend {
    pub fn discover(config: LinuxInputConfig) -> Result<Self, LinuxInputError> {
        let config = config.validate()?;
        let gpio = discover_role(&config, DeviceRole::GpioKeys, &config.gpio)?;
        let rotary = discover_role(&config, DeviceRole::Rotary, &config.rotary)?;
        let touch = discover_role(&config, DeviceRole::Touch, &config.touch)?;
        let mut devices = [
            RoleDevice {
                role: DeviceRole::GpioKeys,
                device: gpio,
            },
            RoleDevice {
                role: DeviceRole::Rotary,
                device: rotary,
            },
            RoleDevice {
                role: DeviceRole::Touch,
                device: touch,
            },
        ];
        if config.grab_policy == GrabPolicy::Exclusive {
            for index in 0..devices.len() {
                if let Err(source) = devices[index].device.grab() {
                    for device in &mut devices[..index] {
                        let _ = device.device.ungrab();
                    }
                    return Err(LinuxInputError::Grab {
                        role: devices[index].role,
                        source,
                    });
                }
            }
        }
        Ok(Self {
            parser: InputParser::new(config.parser)?,
            config,
            devices,
            clock: ReceiptClock::new(),
            terminal: None,
            touch_wait: TouchWaitWindow::default(),
        })
    }

    pub const fn grab_policy(&self) -> GrabPolicy {
        self.config.grab_policy
    }

    pub fn take_touch_wait(&mut self) -> TouchWaitWindow {
        std::mem::take(&mut self.touch_wait)
    }

    pub fn wait_with(
        &self,
        other: BorrowedFd<'_>,
        timeout: Duration,
    ) -> Result<bool, LinuxInputError> {
        let readable =
            PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL;
        let mut descriptors = self
            .devices
            .iter()
            .map(|device| PollFd::new(device.device.as_fd(), readable))
            .collect::<Vec<_>>();
        descriptors.push(PollFd::new(other, readable));
        let timeout_ms = u16::try_from(timeout.as_millis())
            .map_err(|_| LinuxInputError::InvalidConfig("input wait cannot be represented"))?;
        match poll(&mut descriptors, timeout_ms) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => return Ok(false),
            Err(error) => return Err(LinuxInputError::Poll(error)),
        }
        Ok(descriptors
            .last()
            .and_then(PollFd::revents)
            .is_some_and(|ready| ready.intersects(readable)))
    }

    pub fn poll(&mut self) -> Result<LinuxInputBatch, LinuxInputError> {
        self.poll_within(self.config.poll_interval)
    }

    pub fn poll_ready(&mut self) -> Result<LinuxInputBatch, LinuxInputError> {
        self.poll_within(Duration::ZERO)
    }

    fn poll_within(&mut self, timeout: Duration) -> Result<LinuxInputBatch, LinuxInputError> {
        if let Some(terminal) = self.terminal {
            return Err(LinuxInputError::Terminal(terminal));
        }
        let mut descriptors = self
            .devices
            .iter()
            .map(|device| {
                PollFd::new(
                    device.device.as_fd(),
                    PollFlags::POLLIN
                        | PollFlags::POLLERR
                        | PollFlags::POLLHUP
                        | PollFlags::POLLNVAL,
                )
            })
            .collect::<Vec<_>>();
        let timeout_ms = u16::try_from(timeout.as_millis()).map_err(|_| {
            LinuxInputError::InvalidConfig("input poll interval cannot be represented")
        })?;
        poll(&mut descriptors, timeout_ms).map_err(LinuxInputError::Poll)?;
        let readiness = descriptors
            .iter()
            .map(|descriptor| descriptor.revents().unwrap_or(PollFlags::empty()))
            .collect::<Vec<_>>();
        drop(descriptors);

        let mut output = Vec::new();
        for (index, ready) in readiness.into_iter().enumerate() {
            if ready.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
                let role = self.devices[index].role;
                let timestamp = self.clock.now();
                output.extend(self.parser.cancel_all(ResetReason::Unplugged, timestamp)?);
                let terminal = LinuxInputTerminal::DeviceUnplugged(role);
                self.terminal = Some(terminal);
                return Ok(LinuxInputBatch {
                    events: output,
                    terminal: Some(terminal),
                });
            }
            if !ready.contains(PollFlags::POLLIN) {
                continue;
            }
            let role = self.devices[index].role;
            let fetched = match self.devices[index].device.fetch_events() {
                Ok(events) => events.collect::<Vec<_>>(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Vec::new(),
                Err(_) => {
                    let timestamp = self.clock.now();
                    output.extend(self.parser.cancel_all(ResetReason::Unplugged, timestamp)?);
                    let terminal = LinuxInputTerminal::DeviceReadFailed(role);
                    self.terminal = Some(terminal);
                    return Ok(LinuxInputBatch {
                        events: output,
                        terminal: Some(terminal),
                    });
                }
            };
            let fetched_at = SystemTime::now();
            for event in fetched {
                if role == DeviceRole::Touch {
                    self.touch_wait.record(
                        fetched_at
                            .duration_since(event.timestamp())
                            .unwrap_or_default(),
                    );
                }
                let timestamp = self.clock.now();
                output.extend(self.parser.process(RawInputEvent::new(
                    role,
                    event.event_type().0,
                    event.code(),
                    event.value(),
                    timestamp,
                ))?);
            }
        }
        output.extend(self.parser.advance_time(self.clock.now())?);
        Ok(LinuxInputBatch {
            events: output,
            terminal: None,
        })
    }

    pub fn cancel(&mut self) -> Result<LinuxInputBatch, LinuxInputError> {
        if let Some(terminal) = self.terminal {
            return Err(LinuxInputError::Terminal(terminal));
        }
        let events = self
            .parser
            .cancel_all(ResetReason::Cancelled, self.clock.now())?;
        self.terminal = Some(LinuxInputTerminal::Cancelled);
        Ok(LinuxInputBatch {
            events,
            terminal: self.terminal,
        })
    }
}

impl Drop for LinuxInputBackend {
    fn drop(&mut self) {
        if self.config.grab_policy == GrabPolicy::Exclusive {
            for device in &mut self.devices {
                let _ = device.device.ungrab();
            }
        }
    }
}

fn discover_role(
    config: &LinuxInputConfig,
    role: DeviceRole,
    selector: &DeviceSelector,
) -> Result<RawDevice, LinuxInputError> {
    if let Some(path) = &selector.explicit_path {
        let device =
            open_raw(path).map_err(|source| LinuxInputError::DeviceAccess { role, source })?;
        if device.name() != Some(selector.exact_kernel_name.as_str()) {
            return Err(LinuxInputError::DeviceMissing(role));
        }
        if !has_required_capabilities(&device, role)? {
            return Err(LinuxInputError::CapabilityMismatch(role));
        }
        return Ok(device);
    }

    let mut candidates = fs::read_dir(&config.input_directory)
        .map_err(LinuxInputError::Enumeration)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_event_node(path.file_name()))
        .collect::<Vec<_>>();
    candidates.sort();
    if candidates.len() > config.maximum_candidates {
        return Err(LinuxInputError::TooManyCandidates);
    }
    let mut exact = Vec::new();
    let mut named_but_incompatible = false;
    for path in candidates {
        let Ok(device) = open_raw(&path) else {
            continue;
        };
        if device.name() != Some(selector.exact_kernel_name.as_str()) {
            continue;
        }
        if has_required_capabilities(&device, role)? {
            exact.push(device);
        } else {
            named_but_incompatible = true;
        }
    }
    match exact.len() {
        0 if named_but_incompatible => Err(LinuxInputError::CapabilityMismatch(role)),
        0 => Err(LinuxInputError::DeviceMissing(role)),
        1 => exact.pop().ok_or(LinuxInputError::DeviceMissing(role)),
        _ => Err(LinuxInputError::DeviceAmbiguous(role)),
    }
}

fn open_raw(path: &Path) -> io::Result<RawDevice> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)?;
    RawDevice::from_fd(file.into())
}

fn has_required_capabilities(
    device: &RawDevice,
    role: DeviceRole,
) -> Result<bool, LinuxInputError> {
    let keys = device
        .supported_keys()
        .map_or_else(BTreeSet::new, |values| {
            values.iter().map(|value| value.0).collect()
        });
    let relative_axes = device
        .supported_relative_axes()
        .map_or_else(BTreeSet::new, |values| {
            values.iter().map(|value| value.0).collect()
        });
    let absolute_axes = device
        .supported_absolute_axes()
        .map_or_else(BTreeSet::new, |values| {
            values.iter().map(|value| value.0).collect()
        });
    let absolute_ranges = if role == DeviceRole::Touch {
        device
            .get_absinfo()
            .map_err(|source| LinuxInputError::DeviceAccess { role, source })?
            .map(|(axis, info)| (axis.0, (info.minimum(), info.maximum())))
            .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let profile = InputDeviceProfile {
        kernel_name: device.name().unwrap_or_default().to_owned(),
        keys,
        relative_axes,
        absolute_axes,
        absolute_ranges,
    };
    Ok(profile.matches_role(role, &profile.kernel_name))
}

fn is_event_node(name: Option<&OsStr>) -> bool {
    name.and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix("event"))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn validate_path(path: &Path) -> Result<(), LinuxInputError> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if !path.is_absolute()
        || bytes.is_empty()
        || bytes.len() > MAXIMUM_PATH_BYTES
        || bytes.contains(&0)
    {
        return Err(LinuxInputError::InvalidConfig(
            "input paths must be bounded absolute paths without NUL",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_and_config_debug_redact_paths() {
        let config = LinuxInputConfig {
            touch: DeviceSelector::at_path("tlsc6x", "/dev/input/event-secret"),
            ..LinuxInputConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("event-secret"));
        assert!(debug.contains("[configured]"));
    }

    #[test]
    fn event_node_filter_does_not_depend_on_event_number() {
        assert!(is_event_node(Some(OsStr::new("event0"))));
        assert!(is_event_node(Some(OsStr::new("event999"))));
        assert!(!is_event_node(Some(OsStr::new("mouse0"))));
        assert!(!is_event_node(Some(OsStr::new("event"))));
    }

    #[test]
    fn bounded_config_rejects_duplicate_names_and_unsafe_paths() {
        let config = LinuxInputConfig {
            rotary: DeviceSelector::named(DEFAULT_GPIO_NAME),
            ..LinuxInputConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(LinuxInputError::InvalidConfig(_))
        ));
        assert!(
            DeviceSelector::at_path("tlsc6x", "relative/event2")
                .validate()
                .is_err()
        );
    }
}
