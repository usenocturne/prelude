#[cfg(target_os = "linux")]
use std::thread;
use std::{
    env, fs,
    fs::OpenOptions,
    io::{Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use platform_superbird::{
    AudioRoutePreference, HeartbeatStamp, HeartbeatStampError, HostBootOutcome, Max20332Error,
    Max20332RegisterSnapshot, PlatformPaths, ProcessRoleCommand, UsbRole, UsbRoleManager,
    UsbRoleTransaction, apply_initial_backlight, apply_manual_backlight,
    process_output_with_timeout,
};
#[cfg(target_os = "linux")]
use platform_superbird::{GpioKey, LinuxGpioKeyReader, LinuxInputConfig};
#[cfg(any(target_os = "linux", test))]
use platform_superbird::{Max20332, RegisterBus};
use preluded::{
    AUDIO_RUNTIME_STATUS_PATH, AudioRuntimeSnapshot, DaemonConfig, RuntimeSettings,
    load_audio_runtime_status,
};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

const APPLE_VENDOR_ID: &str = "05ac";
const GOOGLE_VENDOR_ID: &str = "18d1";
const OTG_TOPOLOGY_MARKER: &str = "/usr/libexec/prelude-topology-otg";
#[cfg(target_os = "linux")]
const MAX20332_ADDRESS: u16 = 0x35;
const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(15);
const MAXIMUM_MAX20332_SNAPSHOT_BYTES: u64 = 128;
const AUDIO_ROUTE_STATE: &str = "/var/lib/prelude/audio-route";
/// Boot-scoped (tmpfs) marker telling the OTA trial watchdog that this boot
/// deliberately stayed in USB device recovery, so it must not reboot it.
const RECOVERY_BOOT_MARKER: &str = "/run/prelude/recovery-boot";
const DAEMON_CONFIG: &str = "/etc/prelude/config.toml";
const INITIAL_BACKLIGHT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
const AUTOMATIC_HOST_BYPASS_KEY: GpioKey = GpioKey::Back;
#[cfg(any(target_os = "linux", test))]
const AUTOMATIC_HOST_BYPASS_SAMPLES: usize = 5;
#[cfg(any(target_os = "linux", test))]
const AUTOMATIC_HOST_BYPASS_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(
    name = "preludectl",
    version,
    about = "Car Thing wired projection control utility"
)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Doctor {
        #[arg(long)]
        json: bool,
    },
    HostTrial {
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(30..=900))]
        timeout: u64,
    },
    HostTrialNextBoot {
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(30..=900))]
        timeout: u64,
    },
    #[command(hide = true)]
    ConsumeHostTrialNextBoot,
    HostConfirm,
    #[command(hide = true)]
    HostBoot,
    #[command(hide = true)]
    AutomaticHostBoot,
    #[command(hide = true)]
    HostHealth,
    #[command(hide = true)]
    HostHealthFailure,
    #[command(hide = true)]
    RecoverIfRequired,
    #[command(hide = true)]
    MfiSelfTest {
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=8))]
        count: u8,
    },
    DeviceMode,
    #[command(hide = true)]
    InitialBacklight {
        #[arg(long, default_value = DAEMON_CONFIG)]
        config: PathBuf,
    },
    AudioRoute {
        #[command(subcommand)]
        command: AudioRouteCommand,
    },
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DiagnosticsCommand {
    Export {
        #[arg(long, default_value = "prelude-diagnostics")]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum AudioRouteCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Set {
        #[arg(value_enum)]
        route: AudioRouteChoice,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AudioRouteChoice {
    UsbDac,
}

#[derive(Debug, Serialize)]
struct AudioRouteReport {
    requested: &'static str,
    runtime: Probe<AudioRuntimeSnapshot>,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    usb_role: Probe<UsbRole>,
    usb_switch: Probe<UsbSwitchReport>,
    usb_topology: Probe<UsbTopologyReport>,
    host_usb_link: Probe<String>,
    max20332_snapshot: Probe<String>,
    projection_device: Probe<ProjectionUsbDevice>,
    ethernet: Probe<String>,
    recovery_access: Probe<String>,
    mfi: Probe<MfiReport>,
    daemon_heartbeat: Probe<u64>,
    audio_runtime: Probe<AudioRuntimeSnapshot>,
    persistent_host_confirmed: bool,
    automatic_host_policy: bool,
    next_boot_host_trial_pending: bool,
    platform_healthy: bool,
    ready_for_confirmation: bool,
}

#[derive(Debug, Serialize)]
struct MfiReport {
    protocol_major: u8,
    protocol_minor: u8,
    device_version: u8,
    certificate_length: usize,
    certificate_serial: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProjectionUsbProtocol {
    CarPlay,
    AndroidAuto,
}

#[derive(Debug, Serialize)]
struct ProjectionUsbDevice {
    protocol: ProjectionUsbProtocol,
    sysfs_node: String,
}

#[derive(Debug, Serialize)]
struct UsbSwitchReport {
    device_id: u8,
    status1: u8,
    control1: u8,
    control2: u8,
    control3: u8,
    vbus_valid: bool,
    data_routed_to_usb: bool,
    charger_detection_disabled: bool,
    ovp_forced_closed: bool,
    ce_forced_high_impedance: bool,
    id_auto_switch_disabled: bool,
    host_electrical_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct UsbTopologyReport {
    root_hubs: usize,
    external_devices: usize,
    external_hubs: usize,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
enum Probe<T> {
    Ready(T),
    Missing(String),
    Error(String),
}

impl<T> Probe<T> {
    const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match run(Arguments::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("preludectl: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Arguments) -> Result<()> {
    let paths = PlatformPaths::default();
    let manager = UsbRoleManager::new(paths.clone(), ProcessRoleCommand::default());
    match arguments.command {
        Command::Doctor { json } => print_doctor(&paths, &manager, json),
        Command::HostTrial { timeout } => {
            let transaction = manager.begin_transaction()?;
            let timeout = Duration::from_secs(timeout);
            let result = transaction
                .prepare_host_trial(timeout)
                .map_err(anyhow::Error::from)
                .and_then(|()| reset_health_failures(&paths))
                .and_then(|()| stop_recovery_gadget())
                .and_then(|()| prepare_usb_data_path(&paths, &transaction))
                .and_then(|()| transaction.host_trial(timeout).map_err(Into::into));
            if let Err(error) = result {
                return restore_device_recovery(&paths, &transaction, error);
            }
            println!(
                "USB host trial started; automatic reversion in {} seconds",
                timeout.as_secs()
            );
            Ok(())
        }
        Command::HostTrialNextBoot { timeout } => {
            let timeout = Duration::from_secs(timeout);
            manager
                .begin_transaction()?
                .schedule_host_trial_next_boot(timeout)?;
            println!(
                "one timed USB host trial scheduled for the next boot ({} seconds)",
                timeout.as_secs()
            );
            Ok(())
        }
        Command::ConsumeHostTrialNextBoot => {
            let transaction = manager.begin_transaction()?;
            let timeout = match transaction.consume_host_trial_next_boot() {
                Ok(Some(timeout)) => timeout,
                Ok(None) => {
                    println!("no next-boot USB host trial is scheduled");
                    return Ok(());
                }
                Err(error) => return restore_device_recovery(&paths, &transaction, error.into()),
            };
            let result = reset_health_failures(&paths)
                .and_then(|()| stop_recovery_gadget())
                .and_then(|()| prepare_usb_data_path(&paths, &transaction))
                .and_then(|()| transaction.host_trial(timeout).map_err(Into::into));
            if let Err(error) = result {
                return restore_device_recovery(&paths, &transaction, error);
            }
            println!(
                "next-boot USB host trial started; automatic reversion in {} seconds",
                timeout.as_secs()
            );
            Ok(())
        }
        Command::HostConfirm => {
            let transaction = manager.begin_transaction()?;
            let report = doctor(&paths, &manager);
            if !report.ready_for_confirmation {
                bail!("host confirmation refused because one or more health gates failed");
            }
            if let Err(error) = transaction.confirm_host() {
                return restore_device_recovery(&paths, &transaction, error.into());
            }
            println!("persistent USB host mode confirmed");
            Ok(())
        }
        Command::HostBoot => {
            let transaction = manager.begin_transaction()?;
            let preparation = transaction.prepare_host_boot()?;
            match preparation {
                HostBootOutcome::NotConfirmed => {
                    note_recovery_boot();
                    let recovery = finish_prepared_device_recovery(&paths, &transaction);
                    if let Err(error) = recovery {
                        let _ = schedule_device_recovery();
                        return Err(error);
                    }
                    println!("persistent host mode is not confirmed; remaining in USB-device mode");
                    return Ok(());
                }
                HostBootOutcome::TimedTrial => {
                    println!(
                        "persistent host activation skipped because a timed host trial is scheduled or active"
                    );
                    return Ok(());
                }
                HostBootOutcome::Activated => {}
            }
            let result = transaction
                .recovery_required()
                .then_some(())
                .context("host boot did not arm USB-device recovery")
                .and_then(|()| stop_recovery_gadget())
                .and_then(|()| prepare_usb_data_path(&paths, &transaction))
                .and_then(|()| transaction.host_boot().map_err(Into::into));
            if let Err(error) = result {
                return restore_device_recovery(&paths, &transaction, error);
            }
            if let Err(error) = transaction.complete_host_activation() {
                return restore_device_recovery(&paths, &transaction, error.into());
            }
            println!("persistent USB host mode activated");
            Ok(())
        }
        Command::AutomaticHostBoot => {
            ensure_automatic_host_policy(Path::new(OTG_TOPOLOGY_MARKER))?;
            let transaction = manager.begin_transaction()?;
            match automatic_host_bypass_requested() {
                Ok(true) => {
                    note_recovery_boot();
                    finish_device_recovery(&paths, &transaction)?;
                    println!(
                        "automatic USB host mode bypassed for this boot because Back was held during startup"
                    );
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    note_recovery_boot();
                    finish_device_recovery(&paths, &transaction)?;
                    println!(
                        "automatic USB host mode bypassed for this boot because the startup button could not be read: {error:#}"
                    );
                    return Ok(());
                }
            }
            let preparation = transaction.prepare_automatic_host_boot()?;
            match preparation {
                HostBootOutcome::NotConfirmed => {
                    note_recovery_boot();
                    let recovery = finish_prepared_device_recovery(&paths, &transaction);
                    if let Err(error) = recovery {
                        let _ = schedule_device_recovery();
                        return Err(error);
                    }
                    println!(
                        "interrupted USB transition recovered; automatic host activation deferred until the next boot"
                    );
                    return Ok(());
                }
                HostBootOutcome::TimedTrial => {
                    println!(
                        "automatic host activation skipped because a timed host trial is scheduled or active"
                    );
                    return Ok(());
                }
                HostBootOutcome::Activated => {}
            }
            let result = transaction
                .recovery_required()
                .then_some(())
                .context("automatic host boot did not arm USB-device recovery")
                .and_then(|()| stop_recovery_gadget())
                .and_then(|()| prepare_usb_data_path(&paths, &transaction))
                .and_then(|()| transaction.host_boot().map_err(Into::into));
            if let Err(error) = result {
                return restore_device_recovery(&paths, &transaction, error);
            }
            if let Err(error) = transaction.complete_host_activation() {
                return restore_device_recovery(&paths, &transaction, error.into());
            }
            println!("automatic OTG USB host mode activated");
            Ok(())
        }
        Command::HostHealth => {
            let _transaction = manager.begin_transaction()?;
            let automatic = automatic_host_policy_enabled(Path::new(OTG_TOPOLOGY_MARKER));
            if !manager.persistent_host_confirmed() && !automatic {
                reset_health_failures(&paths)?;
                println!("persistent host mode is not confirmed; health check skipped");
                return Ok(());
            }
            if automatic && matches!(manager.current(), Ok(UsbRole::Device)) {
                reset_health_failures(&paths)?;
                println!(
                    "automatic host mode is inactive until the next boot; health check skipped"
                );
                return Ok(());
            }
            let report = doctor(&paths, &manager);
            if report.platform_healthy {
                reset_health_failures(&paths)?;
                return Ok(());
            }
            bail!("persistent USB-host platform health check failed")
        }
        Command::HostHealthFailure => {
            let transaction = manager.begin_transaction()?;
            let automatic = automatic_host_policy_enabled(Path::new(OTG_TOPOLOGY_MARKER));
            if (!manager.persistent_host_confirmed() && !automatic)
                || (automatic && matches!(manager.current(), Ok(UsbRole::Device)))
            {
                reset_health_failures(&paths)?;
                return Ok(());
            }
            let failures = record_health_failure(&paths)?;
            if failures >= 3 {
                if let Err(error) = finish_device_recovery(&paths, &transaction) {
                    let _ = schedule_device_recovery();
                    return Err(error);
                }
                reset_health_failures(&paths)?;
                println!(
                    "host health failed {failures} consecutive times; USB-device recovery restored"
                );
            } else {
                println!("host health failure recorded ({failures}/3)");
            }
            Ok(())
        }
        Command::RecoverIfRequired => {
            let transaction = manager.begin_transaction()?;
            if !transaction.recovery_required() {
                println!("USB-device recovery is not required");
                return Ok(());
            }
            if transaction.automatic_revert_pending()? {
                bail!("timed USB-host trial is active; device recovery deferred");
            }
            if let Err(error) = finish_device_recovery(&paths, &transaction) {
                let _ = schedule_device_recovery();
                return Err(error);
            }
            reset_health_failures(&paths)?;
            println!("interrupted USB transition recovered to device mode");
            Ok(())
        }
        Command::MfiSelfTest { count } => mfi_self_test(count),
        Command::DeviceMode => {
            let transaction = manager.begin_transaction()?;
            if let Err(error) = finish_device_recovery(&paths, &transaction) {
                let _ = schedule_device_recovery();
                return Err(error);
            }
            reset_health_failures(&paths)?;
            println!("USB-device mode active; persistent host confirmation cleared");
            Ok(())
        }
        Command::InitialBacklight { config } => initial_backlight(&config),
        Command::AudioRoute { command } => audio_route(command, Path::new(AUDIO_ROUTE_STATE)),
        Command::Diagnostics {
            command: DiagnosticsCommand::Export { output },
        } => export_diagnostics(&paths.state_dir.join("diagnostics"), &output),
    }
}

fn initial_backlight(config_path: &Path) -> Result<()> {
    let config = DaemonConfig::load(config_path)?;
    let defaults = RuntimeSettings::from_config(&config);
    let settings = RuntimeSettings::load(&config.state_dir, defaults).unwrap_or(defaults);
    let mut ambient = config.platform.ambient_light.clone();
    if !ambient.enabled {
        println!("backlight control is off; leaving the backlight unchanged");
        return Ok(());
    }
    if !settings.auto_brightness {
        ambient.brightness_percent = settings.brightness_percent;
        let applied = apply_manual_backlight(&ambient, INITIAL_BACKLIGHT_TIMEOUT)?;
        println!(
            "backlight set to {}/{} for manual brightness {}%",
            applied.brightness, applied.max_brightness, ambient.brightness_percent
        );
        return Ok(());
    }
    let applied = apply_initial_backlight(&ambient, INITIAL_BACKLIGHT_TIMEOUT)?;
    println!(
        "backlight set to {}/{} for ambient light {}",
        applied.brightness, applied.max_brightness, applied.raw
    );
    Ok(())
}

fn audio_route(command: AudioRouteCommand, path: &Path) -> Result<()> {
    match command {
        AudioRouteCommand::Status { json } => {
            let preference = AudioRoutePreference::load(path)?;
            let platform_paths = PlatformPaths::default();
            let runtime = probe_audio_runtime(
                Path::new(AUDIO_RUNTIME_STATUS_PATH),
                &platform_paths.daemon_heartbeat,
            );
            let report = match preference {
                Some(AudioRoutePreference::UsbDac) => AudioRouteReport {
                    requested: "usb_dac",
                    runtime,
                },
                None => AudioRouteReport {
                    requested: "default_config",
                    runtime,
                },
            };
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{report:#?}");
            }
            Ok(())
        }
        AudioRouteCommand::Set { route } => {
            let preference = match route {
                AudioRouteChoice::UsbDac => AudioRoutePreference::UsbDac,
            };
            preference.save(path)?;
            println!(
                "audio route saved; restart preluded.service to apply it to the next CarPlay session"
            );
            Ok(())
        }
    }
}

fn automatic_host_policy_enabled(otg_topology_marker: &Path) -> bool {
    fs::symlink_metadata(otg_topology_marker).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn ensure_automatic_host_policy(otg_topology_marker: &Path) -> Result<()> {
    if automatic_host_policy_enabled(otg_topology_marker) {
        return Ok(());
    }
    bail!("automatic USB-host boot is available only on the immutable direct-OTG image policy")
}

#[cfg(target_os = "linux")]
fn automatic_host_bypass_requested() -> Result<bool> {
    let reader = LinuxGpioKeyReader::discover(LinuxInputConfig::default())
        .context("discover the Car Thing startup button")?;
    held_for_consecutive_samples(
        || {
            reader
                .is_pressed(AUTOMATIC_HOST_BYPASS_KEY)
                .context("read the Car Thing startup button")
        },
        thread::sleep,
        AUTOMATIC_HOST_BYPASS_SAMPLES,
        AUTOMATIC_HOST_BYPASS_SAMPLE_INTERVAL,
    )
}

fn mark_recovery_boot(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, b"recovery\n").with_context(|| format!("write {}", path.display()))
}

/// Recovery must proceed even if the exemption marker cannot be written.
fn note_recovery_boot() {
    if let Err(error) = mark_recovery_boot(Path::new(RECOVERY_BOOT_MARKER)) {
        eprintln!("could not mark this boot as USB recovery: {error:#}");
    }
}

#[cfg(not(target_os = "linux"))]
fn automatic_host_bypass_requested() -> Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn held_for_consecutive_samples(
    mut is_pressed: impl FnMut() -> Result<bool>,
    mut wait: impl FnMut(Duration),
    samples: usize,
    interval: Duration,
) -> Result<bool> {
    if samples == 0 {
        return Ok(false);
    }
    for index in 0..samples {
        if !is_pressed()? {
            return Ok(false);
        }
        if index + 1 < samples {
            wait(interval);
        }
    }
    Ok(true)
}

fn restore_device_recovery(
    paths: &PlatformPaths,
    transaction: &UsbRoleTransaction<'_, ProcessRoleCommand>,
    operation: anyhow::Error,
) -> Result<()> {
    let recovery = finish_device_recovery(paths, transaction);
    match recovery {
        Ok(()) => Err(operation),
        Err(recovery) => {
            let scheduling = schedule_device_recovery();
            if let Err(scheduling) = scheduling {
                bail!(
                    "host setup failed ({operation:#}); USB-device recovery failed ({recovery:#}); scheduling retry failed ({scheduling:#})"
                );
            }
            bail!(
                "host setup failed ({operation:#}); USB-device recovery failed ({recovery:#}); retry scheduled"
            )
        }
    }
}

fn finish_device_recovery(
    paths: &PlatformPaths,
    transaction: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    transaction.device_mode()?;
    finish_prepared_device_recovery(paths, transaction)
}

fn finish_prepared_device_recovery(
    paths: &PlatformPaths,
    transaction: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    restore_usb_data_path(paths, transaction)?;
    start_recovery_gadget()?;
    transaction.complete_device_recovery()?;
    if let Err(error) = complete_usb_data_restore(paths, transaction) {
        if let Err(rearm) = transaction.arm_device_recovery() {
            bail!(
                "device mode and recovery gadget are active, but MAX20332 snapshot cleanup failed ({error:#}) and recovery could not be rearmed ({rearm})"
            );
        }
        return Err(error);
    }
    Ok(())
}

fn schedule_device_recovery() -> Result<()> {
    let unit = "prelude-device-recovery.service";
    if !systemd_unit_installed(unit) {
        bail!("{unit} is not installed");
    }
    let executable = Path::new("/usr/bin/systemctl");
    let mut command = ProcessCommand::new(executable);
    command.args(["start", "--no-block", unit]);
    let output = process_output_with_timeout(&mut command, executable, Duration::from_secs(5))
        .context("schedule USB-device recovery service")?;
    if !output.status.success() {
        bail!(
            "failed to schedule USB-device recovery: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn stop_recovery_gadget() -> Result<()> {
    set_recovery_gadget(false)
}

fn start_recovery_gadget() -> Result<()> {
    set_recovery_gadget(true)
}

fn set_recovery_gadget(enabled: bool) -> Result<()> {
    const UNITS: [&str; 2] = ["superbird-usb-gadget.service", "android-tools-adbd.service"];
    if !UNITS.iter().any(|unit| systemd_unit_installed(unit)) {
        return Ok(());
    }
    let verb = if enabled { "start" } else { "stop" };
    let executable = Path::new("/usr/bin/systemctl");
    let mut command = ProcessCommand::new(executable);
    command.arg(verb);
    if enabled {
        command.args(UNITS);
    } else {
        command.args(UNITS.into_iter().rev());
    }
    let output = process_output_with_timeout(&mut command, executable, Duration::from_secs(10))
        .with_context(|| format!("invoke systemctl to {verb} USB recovery gadget"))?;
    if !output.status.success() {
        bail!(
            "systemctl {verb} USB recovery gadget failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn systemd_unit_installed(unit: &str) -> bool {
    [
        Path::new("/usr/lib/systemd/system"),
        Path::new("/lib/systemd/system"),
        Path::new("/etc/systemd/system"),
    ]
    .iter()
    .any(|directory| directory.join(unit).exists())
}

fn print_doctor(
    paths: &PlatformPaths,
    manager: &UsbRoleManager<ProcessRoleCommand>,
    json: bool,
) -> Result<()> {
    let report = doctor(paths, manager);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{report:#?}");
    }
    if report.ready_for_confirmation {
        Ok(())
    } else {
        bail!("one or more readiness gates failed")
    }
}

fn doctor(paths: &PlatformPaths, manager: &UsbRoleManager<ProcessRoleCommand>) -> DoctorReport {
    let usb_role = match manager.current() {
        Ok(role) => Probe::Ready(role),
        Err(error) => Probe::Error(error.to_string()),
    };
    let usb_switch = probe_usb_switch(paths);
    let usb_topology = probe_usb_topology(Path::new("/sys/bus/usb/devices"));
    let host_usb_link = diagnose_host_usb_link(&usb_role, &usb_switch, &usb_topology);
    let max20332_snapshot = probe_max20332_snapshot(&paths.max20332_snapshot);
    let projection_device = probe_projection_device(Path::new("/sys/bus/usb/devices"));
    let ethernet = probe_ethernet(Path::new("/sys/class/net"));
    let recovery_access = probe_recovery_access(Path::new("/sys/class/net"));
    let mfi = if matches!(
        &projection_device,
        Probe::Ready(ProjectionUsbDevice {
            protocol: ProjectionUsbProtocol::AndroidAuto,
            ..
        })
    ) {
        Probe::Missing("MFi is not required or opened for Android Auto".to_owned())
    } else {
        probe_mfi()
    };
    let daemon_heartbeat = probe_heartbeat(&paths.daemon_heartbeat);
    let audio_runtime = probe_audio_runtime(
        Path::new(AUDIO_RUNTIME_STATUS_PATH),
        &paths.daemon_heartbeat,
    );
    let next_boot_host_trial_pending = manager.next_boot_host_trial_pending();
    let automatic_host_policy = automatic_host_policy_enabled(Path::new(OTG_TOPOLOGY_MARKER));
    let platform_healthy = matches!(usb_role, Probe::Ready(UsbRole::Host))
        && switch_is_healthy(&usb_switch)
        && (host_usb_link.is_ready() || automatic_host_policy)
        && max20332_snapshot.is_ready()
        && daemon_heartbeat.is_ready();
    let phone_authentication_ready = match &projection_device {
        Probe::Ready(ProjectionUsbDevice {
            protocol: ProjectionUsbProtocol::AndroidAuto,
            ..
        }) => true,
        Probe::Ready(ProjectionUsbDevice {
            protocol: ProjectionUsbProtocol::CarPlay,
            ..
        }) => mfi.is_ready(),
        Probe::Missing(_) | Probe::Error(_) => false,
    };
    let ready_for_confirmation = matches!(usb_role, Probe::Ready(UsbRole::Host))
        && switch_is_healthy(&usb_switch)
        && max20332_snapshot.is_ready()
        && projection_device.is_ready()
        && ethernet.is_ready()
        && recovery_access.is_ready()
        && phone_authentication_ready
        && daemon_heartbeat.is_ready()
        && !next_boot_host_trial_pending;
    DoctorReport {
        usb_role,
        usb_switch,
        usb_topology,
        host_usb_link,
        max20332_snapshot,
        projection_device,
        ethernet,
        recovery_access,
        mfi,
        daemon_heartbeat,
        audio_runtime,
        persistent_host_confirmed: manager.persistent_host_confirmed(),
        automatic_host_policy,
        next_boot_host_trial_pending,
        platform_healthy,
        ready_for_confirmation,
    }
}

fn probe_audio_runtime(status_path: &Path, heartbeat_path: &Path) -> Probe<AudioRuntimeSnapshot> {
    match probe_heartbeat(heartbeat_path) {
        Probe::Ready(_) => {}
        Probe::Missing(detail) => return Probe::Missing(detail),
        Probe::Error(detail) => return Probe::Error(detail),
    }
    match load_audio_runtime_status(status_path) {
        Ok(Some(snapshot)) => Probe::Ready(snapshot),
        Ok(None) => Probe::Missing("preluded audio runtime status is absent".to_owned()),
        Err(error) => Probe::Error(error.to_string()),
    }
}

fn probe_usb_topology(root: &Path) -> Probe<UsbTopologyReport> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return Probe::Error(error.to_string()),
    };
    let mut report = UsbTopologyReport {
        root_hubs: 0,
        external_devices: 0,
        external_hubs: 0,
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Probe::Error(error.to_string()),
        };
        let path = entry.path();
        let has_vendor = match path.join("idVendor").try_exists() {
            Ok(exists) => exists,
            Err(error) => return Probe::Error(error.to_string()),
        };
        if !has_vendor {
            continue;
        }
        let values = (|| -> Result<(u8, String)> {
            let class = fs::read_to_string(path.join("bDeviceClass"))
                .with_context(|| format!("read USB device class for {}", path.display()))?;
            let class = u8::from_str_radix(class.trim(), 16)
                .with_context(|| format!("parse USB device class for {}", path.display()))?;
            let devpath = fs::read_to_string(path.join("devpath"))
                .with_context(|| format!("read USB device path for {}", path.display()))?;
            Ok((class, devpath.trim().to_owned()))
        })();
        let (class, devpath) = match values {
            Ok(values) => values,
            Err(error) => return Probe::Error(format!("{error:#}")),
        };
        if devpath == "0" {
            report.root_hubs += 1;
        } else {
            report.external_devices += 1;
            if class == 0x09 {
                report.external_hubs += 1;
            }
        }
    }
    Probe::Ready(report)
}

fn diagnose_host_usb_link(
    role: &Probe<UsbRole>,
    switch: &Probe<UsbSwitchReport>,
    topology: &Probe<UsbTopologyReport>,
) -> Probe<String> {
    match role {
        Probe::Ready(UsbRole::Device) => {
            return Probe::Missing("USB downstream diagnosis requires host role".to_owned());
        }
        Probe::Missing(detail) => return Probe::Missing(detail.clone()),
        Probe::Error(detail) => return Probe::Error(detail.clone()),
        Probe::Ready(UsbRole::Host) => {}
    }
    match switch {
        Probe::Ready(report) if report.vbus_valid && report.host_electrical_configured => {}
        Probe::Ready(_) => {
            return Probe::Missing(
                "USB host role is active but the complete MAX20332 electrical policy is not"
                    .to_owned(),
            );
        }
        Probe::Missing(detail) => return Probe::Missing(detail.clone()),
        Probe::Error(detail) => return Probe::Error(detail.clone()),
    }
    match topology {
        Probe::Ready(report) if report.root_hubs == 0 => Probe::Error(
            "USB host role is active but no host-controller root hub is visible in sysfs"
                .to_owned(),
        ),
        Probe::Ready(report) if report.external_devices == 0 => Probe::Missing(
            "USB host role and the complete MAX20332 policy are active, but no external USB device or hub enumerated; likely upstream USB-C data-role/CC-PD or topology incompatibility. A future legacy/custom bridge does not need a Linux TYPEC-class device: actual downstream USB enumeration is the acceptance gate"
                .to_owned(),
        ),
        Probe::Ready(report) => Probe::Ready(format!(
            "{} external USB device(s) enumerated, including {} hub(s)",
            report.external_devices, report.external_hubs
        )),
        Probe::Missing(detail) => Probe::Missing(detail.clone()),
        Probe::Error(detail) => Probe::Error(detail.clone()),
    }
}

fn switch_is_healthy(probe: &Probe<UsbSwitchReport>) -> bool {
    matches!(
        probe,
        Probe::Ready(report) if report.vbus_valid && report.host_electrical_configured
    )
}

fn probe_max20332_snapshot(path: &Path) -> Probe<String> {
    match load_max20332_snapshot(path) {
        Ok(PersistedMax20332Snapshot::Valid(_)) => {
            Probe::Ready("validated original device-mode register snapshot is persisted".to_owned())
        }
        Ok(PersistedMax20332Snapshot::Missing) => {
            Probe::Missing("no MAX20332 host-transition snapshot is active".to_owned())
        }
        Ok(PersistedMax20332Snapshot::Corrupt) => {
            Probe::Error("MAX20332 persisted register snapshot is malformed".to_owned())
        }
        Err(error) => Probe::Error(format!("{error:#}")),
    }
}

fn probe_projection_device(root: &Path) -> Probe<ProjectionUsbDevice> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return Probe::Error(error.to_string()),
    };
    let mut phones = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let vendor_path = path.join("idVendor");
        let Ok(vendor) = fs::read_to_string(vendor_path) else {
            continue;
        };
        let vendor = vendor.trim();
        let product = fs::read_to_string(path.join("idProduct"))
            .ok()
            .map(|product| product.trim().to_ascii_lowercase());
        let product_id = product
            .as_deref()
            .and_then(|product| u16::from_str_radix(product, 16).ok());
        let product_name = fs::read_to_string(path.join("product"))
            .ok()
            .map(|product| product.trim().to_owned());
        let protocol = if vendor.eq_ignore_ascii_case(APPLE_VENDOR_ID)
            && product_id.is_some_and(is_apple_mobile_product)
            && product_name
                .as_deref()
                .is_none_or(|product| product == "iPhone")
        {
            Some(ProjectionUsbProtocol::CarPlay)
        } else if vendor.eq_ignore_ascii_case(GOOGLE_VENDOR_ID)
            && matches!(
                product.as_deref(),
                Some("2d00" | "2d01" | "2d02" | "2d03" | "2d04" | "2d05")
            )
        {
            Some(ProjectionUsbProtocol::AndroidAuto)
        } else {
            None
        };
        if let Some(protocol) = protocol {
            phones.push(ProjectionUsbDevice {
                protocol,
                sysfs_node: entry.file_name().to_string_lossy().into_owned(),
            });
        }
    }
    match phones.len() {
        0 => Probe::Missing(
            "no iPhone or Android Auto accessory is enumerated in USB host mode".to_owned(),
        ),
        1 => Probe::Ready(phones.remove(0)),
        count => Probe::Error(format!(
            "{count} supported projection phones are enumerated; disconnect all but one"
        )),
    }
}

fn is_apple_mobile_product(product_id: u16) -> bool {
    product_id & 0xff00 == 0x1200 && matches!(product_id & 0x00f0, 0x0090 | 0x00a0)
}

fn probe_ethernet(root: &Path) -> Probe<String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => return Probe::Error(error.to_string()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        if let Some(driver) = supported_ethernet_driver(root, &name) {
            return Probe::Ready(format!("{name} ({driver})"));
        }
    }
    Probe::Missing("no host-mode USB Ethernet interface has carrier".to_owned())
}

fn probe_recovery_access(root: &Path) -> Probe<String> {
    let connection = match env::var("SSH_CONNECTION") {
        Ok(value) => value,
        Err(_) => {
            return Probe::Missing(
                "host confirmation must be invoked through the host-mode Ethernet SSH session"
                    .to_owned(),
            );
        }
    };
    let (client, local) = match parse_ssh_connection(&connection) {
        Ok(connection) => connection,
        Err(error) => return Probe::Error(error),
    };
    let interfaces = match nix::ifaddrs::getifaddrs() {
        Ok(interfaces) => interfaces,
        Err(error) => return Probe::Error(format!("enumerate interface addresses: {error}")),
    };
    for interface in interfaces {
        let Some(address) = interface.address else {
            continue;
        };
        let observed = address
            .as_sockaddr_in()
            .map(|address| IpAddr::V4(address.ip()))
            .or_else(|| {
                address
                    .as_sockaddr_in6()
                    .map(|address| IpAddr::V6(address.ip()))
            });
        if observed == Some(local) {
            if let Some(driver) = supported_ethernet_driver(root, &interface.interface_name) {
                return Probe::Ready(format!(
                    "{} ({driver}) SSH from {client}",
                    interface.interface_name
                ));
            }
            return Probe::Error(format!(
                "SSH server address {local} is not on a supported host-mode USB Ethernet interface"
            ));
        }
    }
    Probe::Error(format!(
        "SSH server address {local} is not assigned to a local interface"
    ))
}

fn parse_ssh_connection(value: &str) -> std::result::Result<(IpAddr, IpAddr), String> {
    let mut fields = value.split_ascii_whitespace();
    let (Some(client), Some(client_port), Some(local), Some(server_port), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return Err("SSH_CONNECTION must contain client IP/port and server IP/port".to_owned());
    };
    let client = client
        .parse::<IpAddr>()
        .map_err(|error| format!("invalid SSH client address: {error}"))?;
    let local = local
        .parse::<IpAddr>()
        .map_err(|error| format!("invalid SSH server address: {error}"))?;
    for (label, value) in [("client", client_port), ("server", server_port)] {
        let port = value
            .parse::<u16>()
            .map_err(|error| format!("invalid SSH {label} port: {error}"))?;
        if port == 0 {
            return Err(format!("SSH {label} port must be nonzero"));
        }
    }
    Ok((client, local))
}

fn supported_ethernet_driver(root: &Path, interface: &str) -> Option<String> {
    let path = root.join(interface);
    let carrier = fs::read_to_string(path.join("carrier")).ok()?;
    if carrier.trim() != "1" {
        return None;
    }
    let driver = fs::read_link(path.join("device/driver"))
        .ok()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    matches!(driver.as_str(), "ax88179_178a" | "r8152").then_some(driver)
}

fn probe_heartbeat(path: &Path) -> Probe<u64> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Probe::Missing("preluded heartbeat is absent".to_owned());
        }
        Err(error) => return Probe::Error(error.to_string()),
    };
    let stamp = match HeartbeatStamp::decode(&contents) {
        Ok(stamp) => stamp,
        Err(error) => return Probe::Error(error.to_string()),
    };
    let current = match HeartbeatStamp::current() {
        Ok(current) => current,
        Err(error) => return Probe::Error(error.to_string()),
    };
    let age = match stamp.age_at(&current) {
        Ok(age) => age,
        Err(HeartbeatStampError::DifferentBoot) => {
            return Probe::Missing("preluded heartbeat belongs to a previous boot".to_owned());
        }
        Err(error) => return Probe::Error(error.to_string()),
    };
    if age <= HEARTBEAT_MAX_AGE {
        Probe::Ready(age.as_secs())
    } else {
        Probe::Missing(format!("preluded heartbeat is {}s old", age.as_secs()))
    }
}

#[cfg(target_os = "linux")]
fn probe_usb_switch(paths: &PlatformPaths) -> Probe<UsbSwitchReport> {
    use platform_superbird::{LinuxRegisterBus, Max20332};

    let result = (|| -> Result<UsbSwitchReport> {
        let bus = LinuxRegisterBus::open(&paths.i2c_bus, MAX20332_ADDRESS)?;
        let status = Max20332::new(bus).inspect()?;
        Ok(UsbSwitchReport {
            device_id: status.device_id,
            status1: status.status1,
            control1: status.control1,
            control2: status.control2,
            control3: status.control3,
            vbus_valid: status.vbus_valid(),
            data_routed_to_usb: status.data_routed_to_usb(),
            charger_detection_disabled: status.charger_detection_disabled(),
            ovp_forced_closed: status.ovp_forced_closed(),
            ce_forced_high_impedance: status.ce_forced_high_impedance(),
            id_auto_switch_disabled: status.id_auto_switch_disabled(),
            host_electrical_configured: status.host_electrical_configured(),
        })
    })();
    match result {
        Ok(report) => Probe::Ready(report),
        Err(error) => Probe::Error(format!("{error:#}")),
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_usb_switch(_paths: &PlatformPaths) -> Probe<UsbSwitchReport> {
    Probe::Missing("MAX20332 probing is available only on the Linux target".to_owned())
}

#[cfg(target_os = "linux")]
fn probe_mfi() -> Probe<MfiReport> {
    let result = (|| -> Result<MfiReport> {
        let mut auth = prelude_mfi::MfiAuth::open_default()?;
        let info = auth.device_info();
        let certificate_length = auth.certificate()?.len();
        let certificate_serial = hex_string(&auth.certificate_serial()?);
        Ok(MfiReport {
            protocol_major: info.protocol_major,
            protocol_minor: info.protocol_minor,
            device_version: info.device_version,
            certificate_length,
            certificate_serial,
        })
    })();
    match result {
        Ok(report) => Probe::Ready(report),
        Err(error) => Probe::Error(format!("{error:#}")),
    }
}

#[cfg(target_os = "linux")]
fn mfi_self_test(count: u8) -> Result<()> {
    use p256::ecdsa::{Signature, signature::hazmat::PrehashVerifier};
    use prelude_mfi::{CHALLENGE_LEN, MfiAuth, RESPONSE_LEN};

    let mut auth = MfiAuth::open_default()?;
    let verifying_key = mfi_verifying_key(&auth.certificate()?)?;
    let mut random = fs::File::open("/dev/urandom").context("open kernel random source")?;
    let mut responses: Vec<[u8; RESPONSE_LEN]> = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let mut challenge = [0_u8; CHALLENGE_LEN];
        random
            .read_exact(&mut challenge)
            .context("read authentication test challenge")?;
        let response = auth.sign(&challenge)?;
        if response.iter().all(|byte| *byte == 0) {
            bail!("MFi coprocessor returned an all-zero signature");
        }
        if responses.contains(&response) {
            bail!("MFi coprocessor repeated a signature for distinct challenges");
        }
        let signature = Signature::from_slice(&response)
            .context("decode MFi coprocessor's raw P-256 signature")?;
        verifying_key
            .verify_prehash(&challenge, &signature)
            .context("verify MFi coprocessor signature against its certificate")?;
        responses.push(response);
    }
    println!("MFi Auth 3.0 completed {count} independent signing operations");
    Ok(())
}

#[cfg(target_os = "linux")]
fn mfi_verifying_key(certificate: &[u8]) -> Result<p256::ecdsa::VerifyingKey> {
    const P256_SPKI_PREFIX: &[u8] = &[
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    const SEC1_KEY_LEN: usize = 65;

    let mut matches = certificate
        .windows(P256_SPKI_PREFIX.len())
        .enumerate()
        .filter_map(|(offset, candidate)| (candidate == P256_SPKI_PREFIX).then_some(offset));
    let offset = matches
        .next()
        .context("MFi certificate has no P-256 SubjectPublicKeyInfo")?;
    if matches.next().is_some() {
        bail!("MFi certificate has multiple P-256 SubjectPublicKeyInfo values");
    }
    let key_start = offset + P256_SPKI_PREFIX.len();
    let key_end = key_start + SEC1_KEY_LEN;
    let public_key = certificate
        .get(key_start..key_end)
        .context("MFi certificate contains a truncated P-256 public key")?;
    p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key)
        .context("MFi certificate does not contain a valid P-256 public key")
}

#[cfg(not(target_os = "linux"))]
fn mfi_self_test(_count: u8) -> Result<()> {
    bail!("MFi self-test is available only on the Linux target")
}

#[cfg(not(target_os = "linux"))]
fn probe_mfi() -> Probe<MfiReport> {
    Probe::Missing("MFi probing is available only on the Linux target".to_owned())
}

#[cfg(target_os = "linux")]
fn prepare_usb_data_path(
    paths: &PlatformPaths,
    _lifecycle: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    use platform_superbird::{LinuxRegisterBus, Max20332};

    let bus = LinuxRegisterBus::open(&paths.i2c_bus, MAX20332_ADDRESS)?;
    let status = prepare_max20332_host(&mut Max20332::new(bus), &paths.max20332_snapshot)?;
    tracing::info!(
        control1 = status.control1,
        control2 = status.control2,
        control3 = status.control3,
        "MAX20332 complete host electrical policy verified"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn prepare_usb_data_path(
    _paths: &PlatformPaths,
    _lifecycle: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    bail!("USB data-path control is available only on the Linux target")
}

#[cfg(target_os = "linux")]
fn restore_usb_data_path(
    paths: &PlatformPaths,
    _lifecycle: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    use platform_superbird::{LinuxRegisterBus, Max20332};

    let bus = LinuxRegisterBus::open(&paths.i2c_bus, MAX20332_ADDRESS)?;
    restore_max20332_device(&mut Max20332::new(bus), &paths.max20332_snapshot)?;
    tracing::info!("MAX20332 exact device-mode register state verified");
    Ok(())
}

#[cfg(target_os = "linux")]
fn complete_usb_data_restore(
    paths: &PlatformPaths,
    _lifecycle: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    let backup = load_max20332_snapshot(&paths.max20332_snapshot)?;
    match remove_max20332_snapshot(&paths.max20332_snapshot) {
        Ok(()) => Ok(()),
        Err(error) => {
            if !paths.max20332_snapshot.exists() {
                if let PersistedMax20332Snapshot::Valid(snapshot) = backup {
                    if let Err(backup_error) =
                        persist_max20332_snapshot(&paths.max20332_snapshot, snapshot)
                    {
                        bail!(
                            "MAX20332 snapshot cleanup failed ({error:#}) and its recovery backup could not be restored ({backup_error:#})"
                        );
                    }
                }
            }
            Err(error)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn restore_usb_data_path(
    _paths: &PlatformPaths,
    _lifecycle: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    bail!("USB data-path restoration is available only on the Linux target")
}

#[cfg(not(target_os = "linux"))]
fn complete_usb_data_restore(
    _paths: &PlatformPaths,
    _lifecycle: &UsbRoleTransaction<'_, ProcessRoleCommand>,
) -> Result<()> {
    bail!("USB data-path restoration is available only on the Linux target")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedMax20332Snapshot {
    Missing,
    Valid(Max20332RegisterSnapshot),
    Corrupt,
}

fn load_max20332_snapshot(path: &Path) -> Result<PersistedMax20332Snapshot> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistedMax20332Snapshot::Missing);
        }
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let mut contents = Vec::new();
    file.take(MAXIMUM_MAX20332_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("read {}", path.display()))?;
    if contents.len() as u64 > MAXIMUM_MAX20332_SNAPSHOT_BYTES {
        return Ok(PersistedMax20332Snapshot::Corrupt);
    }
    Ok(match Max20332RegisterSnapshot::decode(&contents) {
        Ok(snapshot) => PersistedMax20332Snapshot::Valid(snapshot),
        Err(Max20332Error::InvalidSnapshot) => PersistedMax20332Snapshot::Corrupt,
        Err(error) => return Err(error.into()),
    })
}

#[cfg(any(target_os = "linux", test))]
fn persist_max20332_snapshot(path: &Path, snapshot: Max20332RegisterSnapshot) -> Result<()> {
    atomic_write(path, &snapshot.encode())
}

#[cfg(any(target_os = "linux", test))]
fn remove_max20332_snapshot(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            let parent = path
                .parent()
                .with_context(|| format!("{} has no parent directory", path.display()))?;
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| format!("durably remove {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(any(target_os = "linux", test))]
fn prepare_max20332_host<B: RegisterBus>(
    switch: &mut Max20332<B>,
    snapshot_path: &Path,
) -> Result<platform_superbird::Max20332Status> {
    let live = switch.register_snapshot()?;
    let original = match load_max20332_snapshot(snapshot_path)? {
        PersistedMax20332Snapshot::Missing => {
            if switch.inspect()?.host_electrical_configured() {
                bail!("MAX20332 host-configured live state cannot become its own device snapshot");
            }
            persist_max20332_snapshot(snapshot_path, live)?;
            live
        }
        PersistedMax20332Snapshot::Valid(original) if live == original => original,
        PersistedMax20332Snapshot::Valid(original) if live == original.host_configuration() => {
            let status = switch.inspect()?;
            if !status.host_electrical_configured() {
                bail!("MAX20332 live state does not satisfy the complete host electrical policy");
            }
            return Ok(status);
        }
        PersistedMax20332Snapshot::Corrupt => return Err(Max20332Error::InvalidSnapshot.into()),
        PersistedMax20332Snapshot::Valid(original) => {
            switch.restore(original)?;
            original
        }
    };
    switch.configure_host(original).map_err(Into::into)
}

#[cfg(any(target_os = "linux", test))]
fn restore_max20332_device<B: RegisterBus>(
    switch: &mut Max20332<B>,
    snapshot_path: &Path,
) -> Result<()> {
    let live = switch.register_snapshot()?;
    let original = match load_max20332_snapshot(snapshot_path)? {
        PersistedMax20332Snapshot::Valid(original) => original,
        PersistedMax20332Snapshot::Missing if live.registers_are_target_power_on_defaults() => {
            return Ok(());
        }
        PersistedMax20332Snapshot::Missing => {
            bail!("MAX20332 exact device restore refused because its original snapshot is absent")
        }
        PersistedMax20332Snapshot::Corrupt => {
            return Err(Max20332Error::InvalidSnapshot.into());
        }
    };
    switch.restore(original).map_err(Into::into)
}

fn record_health_failure(paths: &PlatformPaths) -> Result<u8> {
    fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("create {}", paths.state_dir.display()))?;
    let previous = match fs::read_to_string(&paths.health_failures) {
        Ok(value) => value
            .trim()
            .parse::<u8>()
            .with_context(|| format!("parse {}", paths.health_failures.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", paths.health_failures.display()));
        }
    };
    let current = previous.saturating_add(1);
    atomic_write(&paths.health_failures, format!("{current}\n").as_bytes())?;
    Ok(current)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("atomically write {}", path.display()));
    }
    Ok(())
}

fn reset_health_failures(paths: &PlatformPaths) -> Result<()> {
    match fs::remove_file(&paths.health_failures) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn export_diagnostics(source: &Path, output: &Path) -> Result<()> {
    if !source.is_dir() {
        bail!("diagnostic directory {} does not exist", source.display());
    }
    if output.exists() {
        bail!("output path {} already exists", output.display());
    }
    copy_directory(source, output)?;
    println!("diagnostics exported to {}", output.display());
    Ok(())
}

fn copy_directory(source: &Path, output: &Path) -> Result<()> {
    fs::create_dir_all(output)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let destination = output.join(entry.file_name());
        if file_type.is_dir() {
            copy_directory(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn hex_string(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn mark_recovery_boot_creates_marker() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("run/prelude/recovery-boot");
        mark_recovery_boot(&marker).expect("mark recovery boot");
        assert!(marker.is_file());
        mark_recovery_boot(&marker).expect("marking twice is harmless");
    }

    #[test]
    fn automatic_host_bypass_requires_a_continuous_hold() {
        let mut reads = [true, true, true, true, true].into_iter();
        let mut waits = Vec::new();
        assert!(
            held_for_consecutive_samples(
                || Ok(reads.next().expect("bounded key sample")),
                |duration| waits.push(duration),
                AUTOMATIC_HOST_BYPASS_SAMPLES,
                AUTOMATIC_HOST_BYPASS_SAMPLE_INTERVAL,
            )
            .expect("held button")
        );
        assert_eq!(waits.len(), AUTOMATIC_HOST_BYPASS_SAMPLES - 1);
        assert!(
            waits
                .iter()
                .all(|duration| *duration == AUTOMATIC_HOST_BYPASS_SAMPLE_INTERVAL)
        );

        let mut reads = [true, true, false, true, true].into_iter();
        let mut wait_count = 0;
        assert!(
            !held_for_consecutive_samples(
                || Ok(reads.next().expect("bounded key sample")),
                |_| wait_count += 1,
                AUTOMATIC_HOST_BYPASS_SAMPLES,
                AUTOMATIC_HOST_BYPASS_SAMPLE_INTERVAL,
            )
            .expect("released button")
        );
        assert_eq!(wait_count, 2);
    }

    #[test]
    fn automatic_host_bypass_propagates_input_failures() {
        let error = held_for_consecutive_samples(
            || bail!("input unavailable"),
            |_| {},
            AUTOMATIC_HOST_BYPASS_SAMPLES,
            AUTOMATIC_HOST_BYPASS_SAMPLE_INTERVAL,
        )
        .expect_err("input failure");
        assert!(error.to_string().contains("input unavailable"));
    }

    #[derive(Debug)]
    struct SnapshotAwareBus {
        registers: BTreeMap<u8, u8>,
        writes: Vec<(u8, u8)>,
        write_attempts: usize,
        fail_write_attempts: BTreeSet<usize>,
        required_snapshot: Option<PathBuf>,
    }

    impl SnapshotAwareBus {
        fn device_defaults(required_snapshot: Option<PathBuf>) -> Self {
            Self {
                registers: BTreeMap::from([
                    (platform_superbird::DEVICE_ID, 0x2f),
                    (platform_superbird::STATUS1, 0x12),
                    (
                        platform_superbird::CONTROL1,
                        platform_superbird::CONTROL1_POWER_ON_DEFAULT,
                    ),
                    (
                        platform_superbird::CONTROL2,
                        platform_superbird::CONTROL2_POWER_ON_DEFAULT,
                    ),
                    (
                        platform_superbird::CONTROL3,
                        platform_superbird::CONTROL3_POWER_ON_DEFAULT,
                    ),
                ]),
                writes: Vec::new(),
                write_attempts: 0,
                fail_write_attempts: BTreeSet::new(),
                required_snapshot,
            }
        }

        fn set_snapshot(&mut self, snapshot: Max20332RegisterSnapshot) {
            self.registers
                .insert(platform_superbird::DEVICE_ID, snapshot.device_id);
            self.registers
                .insert(platform_superbird::CONTROL1, snapshot.control1);
            self.registers
                .insert(platform_superbird::CONTROL2, snapshot.control2);
            self.registers
                .insert(platform_superbird::CONTROL3, snapshot.control3);
        }

        fn snapshot(&self) -> Max20332RegisterSnapshot {
            Max20332RegisterSnapshot {
                device_id: self.registers[&platform_superbird::DEVICE_ID],
                control1: self.registers[&platform_superbird::CONTROL1],
                control2: self.registers[&platform_superbird::CONTROL2],
                control3: self.registers[&platform_superbird::CONTROL3],
            }
        }
    }

    impl RegisterBus for SnapshotAwareBus {
        fn read_byte(&mut self, register: u8) -> Result<u8, Max20332Error> {
            self.registers
                .get(&register)
                .copied()
                .ok_or_else(|| Max20332Error::Io(format!("missing fake register {register:#04x}")))
        }

        fn write_byte(&mut self, register: u8, value: u8) -> Result<(), Max20332Error> {
            if let Some(path) = &self.required_snapshot {
                if !path.is_file() {
                    return Err(Max20332Error::Io(
                        "register write occurred before durable snapshot".to_owned(),
                    ));
                }
            }
            let attempt = self.write_attempts;
            self.write_attempts += 1;
            if self.fail_write_attempts.remove(&attempt) {
                return Err(Max20332Error::Io(format!(
                    "injected write failure at attempt {attempt}"
                )));
            }
            self.registers.insert(register, value);
            self.writes.push((register, value));
            Ok(())
        }
    }

    #[test]
    fn parses_public_schedule_and_hidden_boot_consumer_commands() {
        let scheduled =
            Arguments::try_parse_from(["preludectl", "host-trial-next-boot", "--timeout", "300"])
                .expect("schedule command");
        assert!(matches!(
            scheduled.command,
            Command::HostTrialNextBoot { timeout: 300 }
        ));
        let consumer = Arguments::try_parse_from(["preludectl", "consume-host-trial-next-boot"])
            .expect("consumer command");
        assert!(matches!(
            consumer.command,
            Command::ConsumeHostTrialNextBoot
        ));
        let automatic = Arguments::try_parse_from(["preludectl", "automatic-host-boot"])
            .expect("automatic OTG boot command");
        assert!(matches!(automatic.command, Command::AutomaticHostBoot));
        let backlight = Arguments::try_parse_from(["preludectl", "initial-backlight"])
            .expect("initial backlight command");
        assert!(matches!(
            backlight.command,
            Command::InitialBacklight { config } if config == Path::new(DAEMON_CONFIG)
        ));
        assert!(
            Arguments::try_parse_from(["preludectl", "host-trial-next-boot", "--timeout", "29",])
                .is_err()
        );
    }

    #[test]
    fn formats_binary_serial_without_exposing_certificate_material() {
        assert_eq!(hex_string(&[0x00, 0x7f, 0xa5, 0xff]), "007fa5ff");
    }

    #[test]
    fn automatic_host_boot_requires_a_regular_otg_policy_marker() {
        let directory = tempfile::tempdir().expect("topology directory");
        let marker = directory.path().join("prelude-topology-otg");
        let target = directory.path().join("marker-target");

        let error = ensure_automatic_host_policy(&marker).expect_err("missing marker");
        assert!(format!("{error:#}").contains("direct-OTG image policy"));

        fs::write(&target, b"otg\n").expect("marker target");
        std::os::unix::fs::symlink(&target, &marker).expect("marker symlink");
        assert!(!automatic_host_policy_enabled(&marker));
        fs::remove_file(&marker).expect("remove marker symlink");

        fs::write(&marker, b"otg\n").expect("regular marker");
        ensure_automatic_host_policy(&marker).expect("regular immutable marker");
    }

    #[test]
    fn max20332_snapshot_precedes_host_writes_and_exact_restore_removes_it() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        let mut switch = Max20332::new(SnapshotAwareBus::device_defaults(Some(path.clone())));
        let original = switch.register_snapshot().expect("original");

        let status = prepare_max20332_host(&mut switch, &path).expect("prepare host");

        assert!(status.host_electrical_configured());
        assert_eq!(
            load_max20332_snapshot(&path).expect("persisted snapshot"),
            PersistedMax20332Snapshot::Valid(original)
        );
        assert_eq!(
            switch.into_inner().writes,
            vec![
                (platform_superbird::CONTROL3, 0x48),
                (platform_superbird::CONTROL2, 0xcc),
                (platform_superbird::CONTROL1, 0x09),
            ]
        );

        let mut restored_bus = SnapshotAwareBus::device_defaults(Some(path.clone()));
        restored_bus.set_snapshot(original.host_configuration());
        let mut restored_switch = Max20332::new(restored_bus);
        restore_max20332_device(&mut restored_switch, &path).expect("restore device");

        assert_eq!(restored_switch.into_inner().snapshot(), original);
        assert!(path.exists());
        remove_max20332_snapshot(&path).expect("completed recovery removes snapshot");
        assert!(!path.exists());
    }

    #[test]
    fn max20332_host_prepare_is_idempotent_with_the_persisted_original() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        let mut switch = Max20332::new(SnapshotAwareBus::device_defaults(Some(path.clone())));

        prepare_max20332_host(&mut switch, &path).expect("first prepare");
        let first_write_count = switch.into_inner().writes.len();

        let original = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let mut retry_bus = SnapshotAwareBus::device_defaults(Some(path.clone()));
        retry_bus.set_snapshot(original.host_configuration());
        let mut retry = Max20332::new(retry_bus);
        prepare_max20332_host(&mut retry, &path).expect("idempotent retry");

        assert_eq!(first_write_count, 3);
        assert!(retry.into_inner().writes.is_empty());
    }

    #[test]
    fn max20332_corrupt_snapshot_fails_closed_outside_known_defaults() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        fs::write(&path, b"corrupt\n").expect("corrupt snapshot");
        let original = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let mut bus = SnapshotAwareBus::device_defaults(None);
        bus.set_snapshot(original.host_configuration());
        let mut switch = Max20332::new(bus);

        let error = prepare_max20332_host(&mut switch, &path)
            .expect_err("host state cannot reconstruct original");

        assert!(format!("{error:#}").contains("malformed"));
        assert!(switch.into_inner().writes.is_empty());
        assert!(path.exists());
    }

    #[test]
    fn max20332_corrupt_snapshot_remains_fail_closed_after_power_cycle() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        fs::write(&path, b"corrupt\n").expect("corrupt snapshot");
        let mut switch = Max20332::new(SnapshotAwareBus::device_defaults(None));

        prepare_max20332_host(&mut switch, &path).expect_err("corrupt host snapshot");
        assert!(switch.into_inner().writes.is_empty());

        let mut defaults = Max20332::new(SnapshotAwareBus::device_defaults(None));
        restore_max20332_device(&mut defaults, &path).expect_err("corrupt recovery snapshot");
        assert!(path.exists());
        assert!(defaults.into_inner().writes.is_empty());
    }

    #[test]
    fn max20332_valid_nondefault_original_survives_power_cycle_and_reconfigure() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        let original = Max20332RegisterSnapshot {
            device_id: 0x2f,
            control1: 0x05,
            control2: 0x9f,
            control3: 0xbe,
        };
        persist_max20332_snapshot(&path, original).expect("authoritative original");
        let mut switch = Max20332::new(SnapshotAwareBus::device_defaults(Some(path.clone())));

        let status = prepare_max20332_host(&mut switch, &path).expect("restore then reconfigure");
        let bus = switch.into_inner();

        assert_eq!(status.register_snapshot(), original.host_configuration());
        assert_eq!(
            load_max20332_snapshot(&path).expect("original retained"),
            PersistedMax20332Snapshot::Valid(original)
        );
        assert_eq!(
            bus.writes,
            vec![
                (platform_superbird::CONTROL1, original.control1),
                (platform_superbird::CONTROL2, original.control2),
                (platform_superbird::CONTROL3, original.control3),
                (
                    platform_superbird::CONTROL3,
                    original.host_configuration().control3,
                ),
                (
                    platform_superbird::CONTROL2,
                    original.host_configuration().control2,
                ),
                (
                    platform_superbird::CONTROL1,
                    original.host_configuration().control1,
                ),
            ]
        );
    }

    #[test]
    fn max20332_valid_nondefault_original_is_restored_after_power_cycle() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        let original = Max20332RegisterSnapshot {
            device_id: 0x2f,
            control1: 0x05,
            control2: 0x9f,
            control3: 0xbe,
        };
        persist_max20332_snapshot(&path, original).expect("authoritative original");
        let mut switch = Max20332::new(SnapshotAwareBus::device_defaults(None));

        restore_max20332_device(&mut switch, &path).expect("exact original restore");

        assert_eq!(switch.into_inner().snapshot(), original);
        assert!(path.exists(), "gadget completion owns durable deletion");
    }

    #[test]
    fn max20332_missing_snapshot_rejects_live_host_policy() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("missing-snapshot");
        let original = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let mut bus = SnapshotAwareBus::device_defaults(None);
        bus.set_snapshot(original.host_configuration());
        let mut switch = Max20332::new(bus);

        prepare_max20332_host(&mut switch, &path)
            .expect_err("host state must not be captured as device original");

        assert!(!path.exists());
        assert!(switch.into_inner().writes.is_empty());
    }

    #[test]
    fn max20332_restore_without_snapshot_never_invents_device_values() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("missing-snapshot");
        let original = Max20332RegisterSnapshot::target_power_on_defaults(0x2f);
        let mut bus = SnapshotAwareBus::device_defaults(None);
        bus.set_snapshot(original.host_configuration());
        let mut switch = Max20332::new(bus);

        let error = restore_max20332_device(&mut switch, &path)
            .expect_err("missing original must fail closed");

        assert!(format!("{error:#}").contains("original snapshot is absent"));
        assert!(switch.into_inner().writes.is_empty());
    }

    #[test]
    fn max20332_failed_host_write_rolls_back_but_retains_snapshot_for_recovery() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        let mut bus = SnapshotAwareBus::device_defaults(Some(path.clone()));
        let original = bus.snapshot();
        bus.fail_write_attempts.insert(1);
        let mut switch = Max20332::new(bus);

        prepare_max20332_host(&mut switch, &path).expect_err("injected fault");

        assert_eq!(switch.into_inner().snapshot(), original);
        assert_eq!(
            load_max20332_snapshot(&path).expect("recovery snapshot"),
            PersistedMax20332Snapshot::Valid(original)
        );
    }

    #[test]
    fn max20332_snapshot_diagnostics_are_bounded_and_redacted() {
        let directory = tempfile::tempdir().expect("state directory");
        let path = directory.path().join("max20332-device-snapshot");
        assert!(matches!(probe_max20332_snapshot(&path), Probe::Missing(_)));
        persist_max20332_snapshot(
            &path,
            Max20332RegisterSnapshot::target_power_on_defaults(0x2f),
        )
        .expect("snapshot");
        let Probe::Ready(detail) = probe_max20332_snapshot(&path) else {
            panic!("valid snapshot diagnostic");
        };
        assert!(!detail.contains("2f") && !detail.contains("0d"));
        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAXIMUM_MAX20332_SNAPSHOT_BYTES + 1).expect("size")],
        )
        .expect("oversized snapshot");
        assert!(matches!(probe_max20332_snapshot(&path), Probe::Error(_)));
    }

    #[test]
    fn usb_switch_requires_vbus_and_complete_host_policy() {
        let report = |vbus_valid, host_electrical_configured| {
            Probe::Ready(UsbSwitchReport {
                device_id: 0x2f,
                status1: 0,
                control1: 0,
                control2: 0,
                control3: 0,
                vbus_valid,
                data_routed_to_usb: host_electrical_configured,
                charger_detection_disabled: host_electrical_configured,
                ovp_forced_closed: host_electrical_configured,
                ce_forced_high_impedance: host_electrical_configured,
                id_auto_switch_disabled: host_electrical_configured,
                host_electrical_configured,
            })
        };

        assert!(switch_is_healthy(&report(true, true)));
        assert!(!switch_is_healthy(&report(false, true)));
        assert!(!switch_is_healthy(&report(true, false)));
        assert!(!switch_is_healthy(&Probe::Missing(
            "not present".to_owned()
        )));
    }

    fn write_usb_topology_device(root: &Path, name: &str, devpath: &str, class: &str) {
        let path = root.join(name);
        fs::create_dir(&path).expect("USB topology node");
        fs::write(path.join("idVendor"), "1d6b\n").expect("USB vendor");
        fs::write(path.join("idProduct"), "0002\n").expect("USB product");
        fs::write(path.join("devpath"), format!("{devpath}\n")).expect("USB path");
        fs::write(path.join("bDeviceClass"), format!("{class}\n")).expect("USB class");
    }

    fn healthy_switch_probe() -> Probe<UsbSwitchReport> {
        Probe::Ready(UsbSwitchReport {
            device_id: 0x2f,
            status1: 0x12,
            control1: 0x09,
            control2: 0xcc,
            control3: 0x48,
            vbus_valid: true,
            data_routed_to_usb: true,
            charger_detection_disabled: true,
            ovp_forced_closed: true,
            ce_forced_high_impedance: true,
            id_auto_switch_disabled: true,
            host_electrical_configured: true,
        })
    }

    #[test]
    fn usb_topology_separates_root_external_and_hub_devices() {
        let directory = tempfile::tempdir().expect("USB sysfs fixture");
        write_usb_topology_device(directory.path(), "usb1", "0", "09");
        write_usb_topology_device(directory.path(), "1-1", "1", "09");
        write_usb_topology_device(directory.path(), "1-1.1", "1.1", "00");

        let Probe::Ready(report) = probe_usb_topology(directory.path()) else {
            panic!("valid USB topology");
        };

        assert_eq!(
            report,
            UsbTopologyReport {
                root_hubs: 1,
                external_devices: 2,
                external_hubs: 1,
            }
        );
    }

    fn write_projection_usb_device(
        root: &Path,
        name: &str,
        vendor: &str,
        product: &str,
        product_name: &str,
    ) {
        let path = root.join(name);
        fs::create_dir(&path).expect("projection USB node");
        fs::write(path.join("idVendor"), format!("{vendor}\n")).expect("USB vendor");
        fs::write(path.join("idProduct"), format!("{product}\n")).expect("USB product");
        fs::write(path.join("product"), format!("{product_name}\n")).expect("USB product name");
    }

    #[test]
    fn projection_device_probe_accepts_one_protocol_and_rejects_ambiguity() {
        let directory = tempfile::tempdir().expect("USB sysfs fixture");
        write_projection_usb_device(
            directory.path(),
            "3-1",
            "05ac",
            "110a",
            "USB-C to 3.5mm Headphone Jack Adapter",
        );
        assert!(matches!(
            probe_projection_device(directory.path()),
            Probe::Missing(_)
        ));

        write_projection_usb_device(directory.path(), "3-2", "18d1", "2d00", "Android Auto");
        assert!(matches!(
            probe_projection_device(directory.path()),
            Probe::Ready(ProjectionUsbDevice {
                protocol: ProjectionUsbProtocol::AndroidAuto,
                ..
            })
        ));

        write_projection_usb_device(directory.path(), "3-3", "05ac", "12a8", "iPad");
        assert!(matches!(
            probe_projection_device(directory.path()),
            Probe::Ready(ProjectionUsbDevice {
                protocol: ProjectionUsbProtocol::AndroidAuto,
                ..
            })
        ));
        fs::write(directory.path().join("3-3/product"), "iPhone\n").expect("iPhone product name");
        assert!(matches!(
            probe_projection_device(directory.path()),
            Probe::Error(detail) if detail.contains("disconnect all but one")
        ));
    }

    #[test]
    fn host_policy_without_downstream_enumeration_reports_topology_failure() {
        let diagnosis = diagnose_host_usb_link(
            &Probe::Ready(UsbRole::Host),
            &healthy_switch_probe(),
            &Probe::Ready(UsbTopologyReport {
                root_hubs: 1,
                external_devices: 0,
                external_hubs: 0,
            }),
        );
        let Probe::Missing(detail) = diagnosis else {
            panic!("missing downstream diagnosis");
        };

        assert!(detail.contains("likely upstream USB-C data-role/CC-PD"));
        assert!(detail.contains("does not need a Linux TYPEC-class device"));
        assert!(detail.contains("actual downstream USB enumeration is the acceptance gate"));
    }

    #[test]
    fn external_device_is_sufficient_without_an_enumerated_hub_or_typec_class() {
        let diagnosis = diagnose_host_usb_link(
            &Probe::Ready(UsbRole::Host),
            &healthy_switch_probe(),
            &Probe::Ready(UsbTopologyReport {
                root_hubs: 1,
                external_devices: 1,
                external_hubs: 0,
            }),
        );

        assert!(matches!(diagnosis, Probe::Ready(_)));
    }

    #[test]
    fn host_role_without_root_hub_is_not_misdiagnosed_as_a_cc_failure() {
        let diagnosis = diagnose_host_usb_link(
            &Probe::Ready(UsbRole::Host),
            &healthy_switch_probe(),
            &Probe::Ready(UsbTopologyReport {
                root_hubs: 0,
                external_devices: 0,
                external_hubs: 0,
            }),
        );

        assert!(matches!(diagnosis, Probe::Error(detail) if detail.contains("root hub")));
    }

    #[test]
    fn parses_ipv4_and_ipv6_ssh_recovery_sessions() {
        assert_eq!(
            parse_ssh_connection("192.0.2.10 54000 192.0.2.20 22").expect("IPv4"),
            (
                "192.0.2.10".parse().expect("client address"),
                "192.0.2.20".parse().expect("server address")
            )
        );
        assert_eq!(
            parse_ssh_connection("2001:db8::1 54000 fe80::20 2222").expect("IPv6"),
            (
                "2001:db8::1".parse().expect("client address"),
                "fe80::20".parse().expect("server address")
            )
        );
        assert!(parse_ssh_connection("192.0.2.10 0 192.0.2.20 22").is_err());
        assert!(parse_ssh_connection("not-an-address 5 192.0.2.20 22").is_err());
        assert!(parse_ssh_connection("192.0.2.10 5 192.0.2.20").is_err());
        assert!(parse_ssh_connection("192.0.2.10 5 192.0.2.20 22 extra").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ethernet_recovery_gate_requires_carrier_and_supported_driver() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("network fixture");
        let interface = directory.path().join("enx0");
        fs::create_dir_all(interface.join("device")).expect("device directory");
        fs::write(interface.join("carrier"), "1\n").expect("carrier");
        symlink("/drivers/ax88179_178a", interface.join("device/driver")).expect("driver symlink");
        assert_eq!(
            supported_ethernet_driver(directory.path(), "enx0").as_deref(),
            Some("ax88179_178a")
        );

        fs::write(interface.join("carrier"), "0\n").expect("carrier down");
        assert!(supported_ethernet_driver(directory.path(), "enx0").is_none());
    }

    #[test]
    fn audio_route_command_persists_usb_dac_state() {
        let directory = tempfile::tempdir().expect("state fixture");
        let path = directory.path().join("audio-route");
        audio_route(
            AudioRouteCommand::Set {
                route: AudioRouteChoice::UsbDac,
            },
            &path,
        )
        .expect("USB route");
        assert_eq!(
            AudioRoutePreference::load(&path).expect("load"),
            Some(AudioRoutePreference::UsbDac)
        );
    }

    #[test]
    fn audio_route_cli_exposes_only_usb_dac() {
        assert!(Arguments::try_parse_from(["preludectl", "audio-route", "set", "usb-dac"]).is_ok());
        assert!(
            Arguments::try_parse_from(["preludectl", "audio-route", "set", "wireless"]).is_err()
        );
    }

    #[test]
    fn audio_runtime_probe_requires_fresh_heartbeat_and_valid_private_status() {
        let directory = tempfile::tempdir().expect("status fixture");
        let heartbeat = directory.path().join("daemon-heartbeat");
        let status = directory.path().join("audio-runtime.json");
        assert!(matches!(
            probe_audio_runtime(&status, &heartbeat),
            Probe::Missing(_)
        ));

        fs::write(
            &heartbeat,
            b"version=1\nboot_id=other-boot\nboottime_millis=1\n",
        )
        .expect("stale heartbeat");
        preluded::AudioStatusWriter::new(&status)
            .write(&AudioRuntimeSnapshot::waiting(
                preluded::AudioRuntimeRoute::UsbDac,
            ))
            .expect("status");
        assert!(matches!(
            probe_audio_runtime(&status, &heartbeat),
            Probe::Missing(_)
        ));

        fs::write(
            &heartbeat,
            HeartbeatStamp::current().expect("current stamp").encode(),
        )
        .expect("fresh heartbeat");
        assert!(matches!(
            probe_audio_runtime(&status, &heartbeat),
            Probe::Ready(AudioRuntimeSnapshot {
                pipeline_state: preluded::AudioPipelineState::WaitingForStream,
                ..
            })
        ));

        fs::write(&status, b"{}\n").expect("corrupt status");
        fs::set_permissions(&status, std::fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(matches!(
            probe_audio_runtime(&status, &heartbeat),
            Probe::Error(_)
        ));
    }
}
