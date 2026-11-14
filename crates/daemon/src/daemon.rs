use crate::{
    AudioBackend, AudioRuntimeSnapshot, AudioStatusWriter, DaemonConfig, DiagnosticEvent,
    DiagnosticSnapshot, Diagnostics, Error, HealthRegistry, HealthState, HeartbeatWriter,
    ProjectionDevice, ProjectionDeviceDiscovery, ProjectionMode, Result, VideoBackend,
};
use prelude_control::{ImplementationStatus, ProjectionCapabilities};
use std::time::{Duration, Instant};

const USB_RESTART_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProbe {
    pub state: HealthState,
    pub detail: String,
    pub transport_state: HealthState,
    pub transport_detail: String,
    pub link_state: HealthState,
    pub link_detail: String,
    pub control_state: HealthState,
    pub control_detail: String,
    pub authentication_state: HealthState,
    pub authentication_detail: String,
    pub session_state: HealthState,
    pub session_detail: String,
    pub implementation: ImplementationStatus,
    pub capabilities: ProjectionCapabilities,
}

pub trait ProjectionRuntime: std::fmt::Debug + Send {
    fn probe(&mut self) -> RuntimeProbe;
    fn set_phone_presence(&mut self, _present: bool) {}
    fn audio_status(&mut self) -> AudioRuntimeSnapshot {
        AudioRuntimeSnapshot::disabled()
    }
    fn play_audio_test_tone(&mut self) -> Result<()> {
        Err(Error::NotReady {
            component: "audio mixer",
            state: "not running",
        })
    }
    fn start(&mut self, device: &ProjectionDevice) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn yields_to(&self, _device: &ProjectionDevice) -> bool {
        false
    }
    fn holds_usb_session(&mut self) -> bool {
        false
    }
    fn begin_handoff(&mut self) {}
    fn stop_for_handoff(&mut self) -> Result<()> {
        self.stop()
    }
}

#[derive(Debug, Default)]
pub struct UnsupportedProjectionRuntime;

impl ProjectionRuntime for UnsupportedProjectionRuntime {
    fn probe(&mut self) -> RuntimeProbe {
        RuntimeProbe {
            state: HealthState::Unsupported,
            detail: "no projection protocol runtime is linked".to_owned(),
            transport_state: HealthState::Unsupported,
            transport_detail: "no wired projection transport is linked".to_owned(),
            link_state: HealthState::Unsupported,
            link_detail: "no projection link runtime is linked".to_owned(),
            control_state: HealthState::Unsupported,
            control_detail: "no projection control runtime is linked".to_owned(),
            authentication_state: HealthState::Unsupported,
            authentication_detail: "no projection authentication runtime is linked".to_owned(),
            session_state: HealthState::Unsupported,
            session_detail: "no projection session runtime is linked".to_owned(),
            implementation: ImplementationStatus::Unsupported,
            capabilities: ProjectionCapabilities::default(),
        }
    }

    fn start(&mut self, _device: &ProjectionDevice) -> Result<()> {
        Err(Error::Unsupported(
            "no projection protocol runtime is linked",
        ))
    }

    fn stop(&mut self) -> Result<()> {
        Err(Error::Unsupported(
            "no projection protocol runtime is linked",
        ))
    }
}

#[derive(Debug)]
pub struct Daemon<D, P> {
    config: DaemonConfig,
    discovery: D,
    projection: P,
    devices: Vec<ProjectionDevice>,
    usb_session: bool,
    usb_restart_until: Option<Instant>,
    health: HealthRegistry,
    diagnostics: Diagnostics,
    heartbeat: HeartbeatWriter,
    audio_status: AudioStatusWriter,
}

impl<D: ProjectionDeviceDiscovery, P: ProjectionRuntime> Daemon<D, P> {
    pub fn new(config: DaemonConfig, discovery: D, projection: P) -> Result<Self> {
        config.validate()?;
        let heartbeat = HeartbeatWriter::new(config.state_dir.clone());
        #[cfg(not(test))]
        let audio_status = AudioStatusWriter::new(crate::AUDIO_RUNTIME_STATUS_PATH);
        #[cfg(test)]
        let audio_status = AudioStatusWriter::new(config.state_dir.join("audio-runtime.json"));
        Ok(Self {
            config,
            discovery,
            projection,
            devices: Vec::new(),
            usb_session: false,
            usb_restart_until: None,
            health: HealthRegistry::default(),
            diagnostics: Diagnostics::new(256),
            heartbeat,
            audio_status,
        })
    }

    pub fn poll(&mut self) -> Result<DiagnosticSnapshot> {
        let projection_required = self.config.projection.mode != ProjectionMode::Disabled;
        match self.discovery.scan() {
            Ok(mut devices) => {
                let holds_usb_session = self.projection.holds_usb_session();
                if holds_usb_session {
                    self.usb_restart_until = None;
                }
                self.usb_session = holds_usb_session
                    || self
                        .usb_restart_until
                        .is_some_and(|until| Instant::now() < until);
                if self.usb_session {
                    devices.retain(|device| !device.is_wireless());
                }
                self.devices = devices;
                self.projection.set_phone_presence(!self.devices.is_empty());
                self.health.set(
                    "usb_discovery",
                    projection_required,
                    HealthState::Ready,
                    format!(
                        "{} supported projection phone(s) observed",
                        self.devices.len()
                    ),
                );
            }
            Err(error) => {
                self.devices.clear();
                self.health.set(
                    "usb_discovery",
                    projection_required,
                    HealthState::Failed,
                    error.to_string(),
                );
                self.diagnostics
                    .record(DiagnosticEvent::now("usb_discovery", "libusb scan failed"));
            }
        }

        let (phone_state, phone_detail) = match self.devices.as_slice() {
            _ if !projection_required => {
                (HealthState::Disabled, "projection is disabled".to_owned())
            }
            [] => (
                HealthState::NotReady,
                "no supported iPhone or Android phone is connected".to_owned(),
            ),
            [device] => (
                HealthState::Ready,
                format!("{} USB candidate available", device.protocol_name()),
            ),
            devices => (
                HealthState::Failed,
                format!(
                    "{} supported phones are connected; disconnect all but one",
                    devices.len()
                ),
            ),
        };
        self.health.set(
            "projection_phone",
            projection_required,
            phone_state,
            phone_detail,
        );

        let probe = self.projection.probe();
        self.health.set(
            "projection_runtime",
            projection_required,
            if projection_required {
                probe.state
            } else {
                HealthState::Disabled
            },
            if projection_required {
                probe.detail
            } else {
                "projection is disabled".to_owned()
            },
        );
        self.health.set(
            "projection_transport",
            projection_required,
            if projection_required {
                probe.transport_state
            } else {
                HealthState::Disabled
            },
            if projection_required {
                probe.transport_detail
            } else {
                "projection is disabled".to_owned()
            },
        );
        self.health.set(
            "projection_authentication",
            projection_required,
            if projection_required {
                probe.authentication_state
            } else {
                HealthState::Disabled
            },
            if projection_required {
                probe.authentication_detail
            } else {
                "projection is disabled".to_owned()
            },
        );
        self.health.set(
            "projection_link",
            projection_required,
            if projection_required {
                probe.link_state
            } else {
                HealthState::Disabled
            },
            if projection_required {
                probe.link_detail
            } else {
                "projection is disabled".to_owned()
            },
        );
        self.health.set(
            "projection_control",
            projection_required,
            if projection_required {
                probe.control_state
            } else {
                HealthState::Disabled
            },
            if projection_required {
                probe.control_detail
            } else {
                "projection is disabled".to_owned()
            },
        );
        self.health.set(
            "projection_session",
            projection_required,
            if projection_required {
                probe.session_state
            } else {
                HealthState::Disabled
            },
            if projection_required {
                probe.session_detail
            } else {
                "projection is disabled".to_owned()
            },
        );
        self.update_media_health(projection_required, probe.capabilities);
        let audio_status = self.projection.audio_status();
        self.audio_status
            .write(&audio_status)
            .map_err(|error| Error::AudioStatus {
                path: self.audio_status.path().to_path_buf(),
                detail: error.to_string(),
            })?;
        if let Err(error) = self.heartbeat.write() {
            self.health.set(
                "daemon_heartbeat",
                projection_required,
                HealthState::Failed,
                error.to_string(),
            );
            return Err(error);
        }
        self.health.set(
            "daemon_heartbeat",
            projection_required,
            HealthState::Ready,
            format!("updated {}", self.heartbeat.path().display()),
        );
        Ok(self.diagnostics.snapshot(&self.health))
    }

    pub fn start(&mut self) -> Result<()> {
        if self.config.projection.mode == ProjectionMode::Disabled {
            return Err(Error::Unsupported(
                "projection is disabled by configuration",
            ));
        }
        let device = match self.devices.as_slice() {
            [device] => device,
            [] => {
                return Err(Error::NotReady {
                    component: "projection phone discovery",
                    state: "no_device",
                });
            }
            _ => {
                return Err(Error::NotReady {
                    component: "projection phone discovery",
                    state: "ambiguous_devices",
                });
            }
        };
        self.projection.start(device)
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.config.projection.mode == ProjectionMode::Disabled {
            return Ok(());
        }
        self.projection.stop()?;
        self.audio_status
            .write(&self.projection.audio_status())
            .map_err(|error| Error::AudioStatus {
                path: self.audio_status.path().to_path_buf(),
                detail: error.to_string(),
            })
    }

    pub fn choose_wireless(&mut self) -> Result<()> {
        self.discovery.choose_wireless();
        if self.devices.iter().any(|device| !device.is_wireless()) {
            self.devices.clear();
            self.usb_session = false;
            self.usb_restart_until = None;
            self.stop()?;
        }
        Ok(())
    }

    pub fn maintain_projection(&mut self) -> Result<bool> {
        if !matches!(
            self.config.projection.mode,
            ProjectionMode::Automatic | ProjectionMode::ExperimentalPrivateProtocol
        ) {
            return Ok(false);
        }
        let transport_alive = matches!(
            self.projection.probe().transport_state,
            HealthState::Starting | HealthState::Ready
        );
        if transport_alive {
            let [device] = self.devices.as_slice() else {
                return Ok(false);
            };
            if !self.projection.yields_to(device) {
                return Ok(false);
            }
            tracing::info!(
                protocol = device.protocol_name(),
                "a wired phone was plugged in; replacing the wireless session"
            );
        }
        self.start()?;
        Ok(true)
    }

    pub fn diagnostics_json(&self) -> Result<String> {
        self.diagnostics.snapshot_json(&self.health)
    }

    pub fn polling_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.config.usb.polling_interval_ms)
    }

    pub fn replace_projection_runtime(
        &mut self,
        config: DaemonConfig,
        projection: P,
    ) -> Result<()> {
        config.validate()?;
        if self.config.projection.mode != ProjectionMode::Disabled {
            self.projection.stop()?;
        }
        self.config = config;
        self.projection = projection;
        self.devices
            .retain(|device| !matches!(device, ProjectionDevice::AndroidAuto(_)));
        if self.usb_session {
            self.usb_restart_until = Some(Instant::now() + USB_RESTART_GRACE);
        }
        Ok(())
    }

    pub const fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub fn audio_status_snapshot(&mut self) -> AudioRuntimeSnapshot {
        self.projection.audio_status()
    }

    pub fn play_audio_test_tone(&mut self) -> Result<()> {
        self.projection.play_audio_test_tone()
    }

    pub fn has_discovered_phone(&self) -> bool {
        !self.devices.is_empty()
    }

    pub const fn projecting_over_usb(&self) -> bool {
        self.usb_session
    }

    pub fn has_wireless_phone(&self) -> bool {
        self.devices.iter().any(ProjectionDevice::is_wireless)
    }

    fn update_media_health(
        &mut self,
        projection_required: bool,
        capabilities: ProjectionCapabilities,
    ) {
        let video_configured = self.config.media.video != VideoBackend::None;
        self.health.set(
            "video",
            projection_required,
            media_state(projection_required, video_configured, capabilities.video),
            media_detail(
                projection_required,
                video_configured,
                capabilities.video,
                "video",
            ),
        );
        let audio_configured = self.config.media.audio != AudioBackend::None;
        self.health.set(
            "audio",
            projection_required,
            media_state(projection_required, audio_configured, capabilities.audio),
            media_detail(
                projection_required,
                audio_configured,
                capabilities.audio,
                "audio",
            ),
        );
        let microphone_configured = self.config.media.microphone != AudioBackend::None;
        self.health.set(
            "microphone",
            projection_required && self.config.projection.require_complete_capabilities,
            media_state(
                projection_required,
                microphone_configured,
                capabilities.microphone,
            ),
            media_detail(
                projection_required,
                microphone_configured,
                capabilities.microphone,
                "microphone",
            ),
        );
    }
}

fn media_state(required: bool, configured: bool, negotiated: bool) -> HealthState {
    if !required {
        HealthState::Disabled
    } else if !configured {
        HealthState::Unsupported
    } else if !negotiated {
        HealthState::NotReady
    } else {
        HealthState::Ready
    }
}

fn media_detail(
    required: bool,
    configured: bool,
    negotiated: bool,
    component: &'static str,
) -> String {
    if !required {
        "projection is disabled".to_owned()
    } else if !configured {
        format!("no {component} backend is configured")
    } else if !negotiated {
        format!("projection runtime did not negotiate {component}")
    } else {
        format!("{component} backend and capability are available")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use prelude_usb_carkit::{APPLE_VENDOR_ID, DeviceIdentity, UsbAddress, UsbConfigurationInfo};

    use super::*;

    #[derive(Debug)]
    struct FixedDiscovery(Vec<ProjectionDevice>);

    impl ProjectionDeviceDiscovery for FixedDiscovery {
        fn scan(&mut self) -> Result<Vec<ProjectionDevice>> {
            Ok(self.0.clone())
        }
    }

    #[derive(Debug)]
    struct FailedDiscovery;

    impl ProjectionDeviceDiscovery for FailedDiscovery {
        fn scan(&mut self) -> Result<Vec<ProjectionDevice>> {
            Err(Error::Runtime("transient USB scan failure".to_owned()))
        }
    }

    fn candidate() -> ProjectionDevice {
        ProjectionDevice::CarPlay(prelude_usb_carkit::IphoneCandidate {
            identity: DeviceIdentity {
                address: UsbAddress { bus: 1, address: 2 },
                vendor_id: APPLE_VENDOR_ID,
                product_id: 0x12a8,
                serial_number: Some("not-exported-to-diagnostics".to_owned()),
            },
            active_configuration: Some(1),
            configurations: vec![UsbConfigurationInfo {
                value: 1,
                interfaces: Vec::new(),
            }],
        })
    }

    fn test_config() -> (tempfile::TempDir, DaemonConfig) {
        let directory = tempfile::tempdir().expect("state directory");
        let config = DaemonConfig {
            state_dir: directory.path().to_path_buf(),
            ..DaemonConfig::default()
        };
        (directory, config)
    }

    #[test]
    fn disabled_default_is_diagnostic_only() {
        let (state_directory, config) = test_config();
        let mut daemon = Daemon::new(
            config,
            FixedDiscovery(vec![candidate()]),
            UnsupportedProjectionRuntime,
        )
        .expect("daemon");
        let snapshot = daemon.poll().expect("poll");
        assert_eq!(snapshot.overall, crate::OverallHealth::Disabled);
        assert!(state_directory.path().join("daemon-heartbeat").is_file());
        let audio_status = state_directory.path().join("audio-runtime.json");
        assert_eq!(
            crate::load_audio_runtime_status(&audio_status).expect("audio status"),
            Some(AudioRuntimeSnapshot::disabled())
        );
        assert!(matches!(daemon.start(), Err(Error::Unsupported(_))));
        assert!(
            !daemon
                .diagnostics_json()
                .expect("json")
                .contains("not-exported-to-diagnostics")
        );
    }

    #[test]
    fn enabled_projection_with_placeholder_runtime_is_not_ready() {
        let (_state_directory, mut config) = test_config();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        let mut daemon = Daemon::new(
            config,
            FixedDiscovery(vec![candidate()]),
            UnsupportedProjectionRuntime,
        )
        .expect("daemon");
        assert_eq!(
            daemon.poll().expect("poll").overall,
            crate::OverallHealth::NotReady
        );
        assert!(matches!(daemon.start(), Err(Error::Unsupported(_))));
    }

    #[test]
    fn multiple_supported_phones_fail_closed_before_runtime_selection() {
        let (_state_directory, mut config) = test_config();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        let mut daemon = Daemon::new(
            config,
            FixedDiscovery(vec![candidate(), candidate()]),
            UnsupportedProjectionRuntime,
        )
        .expect("daemon");

        let snapshot = daemon.poll().expect("poll");
        assert_eq!(
            snapshot
                .health
                .components()
                .get("projection_phone")
                .expect("phone health")
                .state,
            HealthState::Failed
        );
        assert!(matches!(
            daemon.start(),
            Err(Error::NotReady {
                component: "projection phone discovery",
                state: "ambiguous_devices"
            })
        ));
    }

    #[test]
    fn poll_fails_before_heartbeat_when_runtime_state_directory_is_unwritable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let state_file = directory.path().join("not-a-directory");
        std::fs::write(&state_file, b"occupied").expect("state file");
        let config = DaemonConfig {
            state_dir: state_file,
            ..DaemonConfig::default()
        };
        let mut daemon = Daemon::new(
            config,
            FixedDiscovery(Vec::new()),
            UnsupportedProjectionRuntime,
        )
        .expect("daemon");
        assert!(matches!(daemon.poll(), Err(Error::AudioStatus { .. })));
    }

    #[test]
    fn transient_discovery_failure_does_not_stale_the_daemon_heartbeat() {
        let (state_directory, config) = test_config();
        let mut daemon =
            Daemon::new(config, FailedDiscovery, UnsupportedProjectionRuntime).expect("daemon");

        let snapshot = daemon.poll().expect("poll remains live");
        assert_eq!(
            snapshot
                .health
                .components()
                .get("usb_discovery")
                .expect("discovery health")
                .state,
            HealthState::Failed
        );
        assert_eq!(
            snapshot
                .health
                .components()
                .get("daemon_heartbeat")
                .expect("heartbeat health")
                .state,
            HealthState::Ready
        );
        assert!(state_directory.path().join("daemon-heartbeat").is_file());
    }

    #[derive(Debug)]
    struct ReconnectRuntime {
        ready: Arc<AtomicBool>,
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }

    impl ProjectionRuntime for ReconnectRuntime {
        fn probe(&mut self) -> RuntimeProbe {
            RuntimeProbe {
                state: HealthState::Unsupported,
                detail: "projection unavailable".to_owned(),
                transport_state: if self.ready.load(Ordering::SeqCst) {
                    HealthState::Ready
                } else {
                    HealthState::NotReady
                },
                transport_detail: "test transport".to_owned(),
                link_state: HealthState::NotReady,
                link_detail: "test link".to_owned(),
                control_state: HealthState::NotReady,
                control_detail: "test control".to_owned(),
                authentication_state: HealthState::NotReady,
                authentication_detail: "test authentication".to_owned(),
                session_state: HealthState::NotReady,
                session_detail: "test session".to_owned(),
                implementation: ImplementationStatus::Unsupported,
                capabilities: ProjectionCapabilities::default(),
            }
        }

        fn start(&mut self, _device: &ProjectionDevice) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.ready.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.ready.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn carrier_maintenance_connects_once_then_observes_readiness() {
        let (_state_directory, mut config) = test_config();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        let ready = Arc::new(AtomicBool::new(false));
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let runtime = ReconnectRuntime {
            ready: Arc::clone(&ready),
            starts: Arc::clone(&starts),
            stops: Arc::clone(&stops),
        };
        let mut daemon =
            Daemon::new(config, FixedDiscovery(vec![candidate()]), runtime).expect("daemon");
        daemon.poll().expect("poll");
        assert!(daemon.maintain_projection().expect("first connect"));
        assert!(!daemon.maintain_projection().expect("already connected"));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn choosing_a_wireless_phone_stops_the_usb_session_without_restarting_it() {
        let (_state_directory, mut config) = test_config();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let runtime = ReconnectRuntime {
            ready: Arc::new(AtomicBool::new(false)),
            starts: Arc::clone(&starts),
            stops: Arc::clone(&stops),
        };
        let mut daemon =
            Daemon::new(config, FixedDiscovery(vec![candidate()]), runtime).expect("daemon");
        daemon.poll().expect("poll");
        assert!(daemon.maintain_projection().expect("connect"));
        daemon.choose_wireless().expect("choose wireless");
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(daemon.maintain_projection().is_err());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[derive(Debug)]
    struct UsbSessionRuntime;

    impl ProjectionRuntime for UsbSessionRuntime {
        fn probe(&mut self) -> RuntimeProbe {
            UnsupportedProjectionRuntime.probe()
        }

        fn start(&mut self, _device: &ProjectionDevice) -> Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn holds_usb_session(&mut self) -> bool {
            true
        }
    }

    #[test]
    fn wireless_phones_stay_hidden_while_a_usb_session_runs() {
        let (_state_directory, config) = test_config();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let stream =
            std::net::TcpStream::connect(listener.local_addr().expect("address")).expect("stream");
        let phones = crate::WirelessPhones::default();
        assert!(phones.offer(stream, "phone".to_owned()));
        let wireless = phones
            .snapshot()
            .into_iter()
            .map(ProjectionDevice::WirelessAndroidAuto)
            .collect::<Vec<_>>();
        let mut daemon =
            Daemon::new(config, FixedDiscovery(wireless), UsbSessionRuntime).expect("daemon");
        daemon.poll().expect("poll");
        assert!(daemon.projecting_over_usb());
        assert!(!daemon.has_wireless_phone());
    }

    #[derive(Debug)]
    struct SwitchableUsbRuntime(Arc<AtomicBool>);

    impl ProjectionRuntime for SwitchableUsbRuntime {
        fn probe(&mut self) -> RuntimeProbe {
            UnsupportedProjectionRuntime.probe()
        }

        fn start(&mut self, _device: &ProjectionDevice) -> Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn holds_usb_session(&mut self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn android_auto_candidate() -> ProjectionDevice {
        ProjectionDevice::AndroidAuto(prelude_android_auto::AoapCandidate {
            device: prelude_android_auto::UsbDeviceSnapshot {
                topology: prelude_android_auto::UsbTopology {
                    bus: 1,
                    port_path: vec![1],
                },
                address: 3,
                vendor_id: 0x18d1,
                product_id: 0x2d00,
                device_class: 0,
                active_configuration: Some(1),
                configurations: Vec::new(),
            },
            mode: prelude_android_auto::AoapMode::Accessory,
        })
    }

    #[test]
    fn runtime_replacement_keeps_the_usb_claim_while_the_wired_session_restarts() {
        let (_state_directory, config) = test_config();
        let holds = Arc::new(AtomicBool::new(true));
        let mut daemon = Daemon::new(
            config.clone(),
            FixedDiscovery(vec![android_auto_candidate()]),
            SwitchableUsbRuntime(Arc::clone(&holds)),
        )
        .expect("daemon");
        daemon.poll().expect("poll");
        assert!(daemon.projecting_over_usb());
        assert!(daemon.has_discovered_phone());

        let restarted = Arc::new(AtomicBool::new(false));
        daemon
            .replace_projection_runtime(config, SwitchableUsbRuntime(Arc::clone(&restarted)))
            .expect("replace runtime");
        assert!(!daemon.has_discovered_phone());
        assert!(daemon.projecting_over_usb());
        daemon.poll().expect("poll");
        assert!(daemon.projecting_over_usb());

        restarted.store(true, Ordering::SeqCst);
        daemon.poll().expect("poll");
        assert!(daemon.projecting_over_usb());
        assert_eq!(daemon.usb_restart_until, None);

        restarted.store(false, Ordering::SeqCst);
        daemon.usb_restart_until = Some(Instant::now());
        daemon.poll().expect("poll");
        assert!(!daemon.projecting_over_usb());
    }

    #[derive(Debug)]
    struct WirelessSessionRuntime {
        starts: Arc<AtomicUsize>,
    }

    impl ProjectionRuntime for WirelessSessionRuntime {
        fn probe(&mut self) -> RuntimeProbe {
            let mut probe = UnsupportedProjectionRuntime.probe();
            probe.transport_state = HealthState::Ready;
            probe
        }

        fn start(&mut self, _device: &ProjectionDevice) -> Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn yields_to(&self, device: &ProjectionDevice) -> bool {
            !device.is_wireless()
        }
    }

    #[test]
    fn a_wired_phone_replaces_a_live_session_that_yields_to_it() {
        let (_state_directory, mut config) = test_config();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        let starts = Arc::new(AtomicUsize::new(0));
        let runtime = WirelessSessionRuntime {
            starts: Arc::clone(&starts),
        };
        let mut daemon =
            Daemon::new(config, FixedDiscovery(vec![candidate()]), runtime).expect("daemon");
        daemon.poll().expect("poll");
        assert!(daemon.maintain_projection().expect("switch to wired"));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_replacement_stops_the_old_session_before_installing_candidate() {
        let (_state_directory, mut config) = test_config();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        let ready = Arc::new(AtomicBool::new(false));
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let runtime = ReconnectRuntime {
            ready: Arc::clone(&ready),
            starts: Arc::clone(&starts),
            stops: Arc::clone(&stops),
        };
        let mut daemon = Daemon::new(config.clone(), FixedDiscovery(vec![candidate()]), runtime)
            .expect("daemon");
        daemon.poll().expect("poll");
        assert!(daemon.maintain_projection().expect("connect"));

        let replacement_ready = Arc::new(AtomicBool::new(false));
        let replacement_starts = Arc::new(AtomicUsize::new(0));
        let replacement = ReconnectRuntime {
            ready: Arc::clone(&replacement_ready),
            starts: Arc::clone(&replacement_starts),
            stops: Arc::new(AtomicUsize::new(0)),
        };
        daemon
            .replace_projection_runtime(config, replacement)
            .expect("replace runtime");

        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(!ready.load(Ordering::SeqCst));
        assert!(daemon.maintain_projection().expect("connect replacement"));
        assert_eq!(replacement_starts.load(Ordering::SeqCst), 1);
    }
}
