use std::{collections::BTreeMap, path::PathBuf};

use clap::Parser;
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use platform_superbird::{AmbientLightMonitor, AmbientLightState};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use preluded::{AudioPipelineState, AudioRuntimeSnapshot, AudioStatusWriter};
use preluded::{
    CombinedDeviceDiscovery, Daemon, DaemonConfig, HealthState, Result, SelectedRuntime,
    WirelessService,
};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use preluded::{
    DisplayPlatform, DisplayStatus, Error, HostUi, HostUiEvent, RuntimeSettings,
    WirelessPhoneDirectory, load_last_display_platform, render_status_screen,
};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "preluded",
    about = "CarPlay and Android Auto daemon for Car Thing"
)]
struct Arguments {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    diagnose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();
    let arguments = Arguments::parse();
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    if !arguments.diagnose
        && let Err(error) = std::thread::Builder::new()
            .name("prelude-fonts".to_owned())
            .spawn(preluded::prepare_status_screens)
    {
        warn!(%error, "preloading fonts failed; status screens load them on first use");
    }
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut config = arguments
        .config
        .map_or_else(|| Ok(DaemonConfig::default()), DaemonConfig::load)?;
    #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
    let mut config = arguments
        .config
        .map_or_else(|| Ok(DaemonConfig::default()), DaemonConfig::load)?;
    // Keep the access point's country and configured passphrase across
    // over-the-air updates; see `wireless_identity`.
    if config.wireless.enabled {
        let identity = preluded::wireless_identity::reconcile(&config.wireless, &config.state_dir);
        preluded::wireless_identity::apply(&identity, &mut config.wireless);
    }
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let settings_base_config = config.clone();
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let runtime_settings = {
        let defaults = RuntimeSettings::from_config(&config);
        match RuntimeSettings::load(&config.state_dir, defaults) {
            Ok(settings) => settings,
            Err(error) => {
                warn!(%error, "persisted host settings are invalid; using configuration defaults");
                defaults
            }
        }
    };
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    runtime_settings
        .apply_to_config(&mut config)
        .map_err(|error| {
            Error::Runtime(format!("applying persisted host settings failed: {error}"))
        })?;
    config.validate()?;
    #[cfg_attr(
        not(all(target_os = "linux", feature = "linux-media")),
        allow(unused_mut)
    )]
    let mut wireless = if arguments.diagnose {
        None
    } else {
        WirelessService::start(&config)?
    };
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let ambient_monitor = if arguments.diagnose {
        None
    } else {
        match AmbientLightMonitor::start(config.platform.ambient_light.clone()) {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                warn!(%error, "ambient-light monitor could not start; using day mode");
                None
            }
        }
    };
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let ambient_light = ambient_monitor
        .as_ref()
        .map_or_else(AmbientLightState::default, AmbientLightMonitor::state);
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let ambient_control = ambient_monitor.as_ref().map(AmbientLightMonitor::control);
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let display = if arguments.diagnose {
        None
    } else {
        let startup_platform = load_last_display_platform(&config.state_dir);
        let startup_overlay = render_status_screen(
            startup_platform.unwrap_or(DisplayPlatform::CarPlay),
            DisplayStatus::WaitingForPhone,
        )
        .map_err(|error| {
            preluded::Error::Runtime(format!("rendering the startup screen failed: {error}"))
        })?;
        let display_runtime =
            prelude_media::LinuxDisplayRuntime::start_with_overlay(startup_overlay.clone())
                .map_err(|error| {
                    preluded::Error::Runtime(format!("starting the local display failed: {error}"))
                })?;
        if startup_platform.is_none() {
            let android_auto_overlay =
                render_status_screen(DisplayPlatform::AndroidAuto, DisplayStatus::WaitingForPhone)
                    .map_err(|error| {
                        preluded::Error::Runtime(format!(
                            "rendering the Android Auto startup screen failed: {error}"
                        ))
                    })?;
            display_runtime
                .handle()
                .start_waiting_overlay_cycle(
                    vec![startup_overlay, android_auto_overlay],
                    std::time::Duration::from_secs(3),
                    std::time::Duration::from_millis(600),
                )
                .map_err(|error| {
                    preluded::Error::Runtime(format!(
                        "starting the startup overlay cycle failed: {error}"
                    ))
                })?;
        }
        Some(display_runtime)
    };
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut runtime = SelectedRuntime::from_config_with_display_and_ambient(
        &config,
        display
            .as_ref()
            .map(prelude_media::LinuxDisplayRuntime::handle),
        ambient_light.clone(),
    )?;
    #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
    let mut runtime = SelectedRuntime::from_config(&config)?;
    let mut discovery = CombinedDeviceDiscovery::new(
        std::time::Duration::from_millis(config.usb.control_transfer_timeout_ms),
        config.android_auto.enable_live_session,
        config.android_auto.android_vendor_ids.clone(),
    );
    if let Some(wireless) = &wireless {
        runtime.attach_wireless(
            &config,
            wireless,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display
                .as_ref()
                .map(prelude_media::LinuxDisplayRuntime::handle),
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            ambient_light.clone(),
            #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
            platform_superbird::AmbientLightState::default(),
        );
        discovery =
            discovery.with_wireless(wireless.android_auto_phones(), wireless.carplay_phones());
        info!("wireless projection is enabled");
    }
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    if let (Some(wireless), Some(display)) = (wireless.as_mut(), display.as_ref()) {
        wireless.show_pairing_codes(display.handle(), config.state_dir.clone())?;
    }
    let mut daemon = Daemon::new(config, discovery, runtime)?;

    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let host_ui = display.as_ref().map(|display_runtime| {
        HostUi::start(
            display_runtime.handle(),
            daemon.config().state_dir.clone(),
            runtime_settings,
            daemon.config().media.equalizer.clone(),
            daemon.config().platform.presets.clone(),
            ambient_control.clone(),
            wireless.as_ref().map(WirelessService::directory),
        )
    });
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let host_ui = match host_ui {
        Some(Ok(host_ui)) => Some(host_ui),
        Some(Err(error)) => return Err(Error::Runtime(error)),
        None => None,
    };
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let host_ui_wake = host_ui.as_ref().map(HostUi::wake);

    if arguments.diagnose {
        daemon.poll()?;
        println!("{}", daemon.diagnostics_json()?);
        return Ok(());
    }

    info!(
        "automatic wired projection daemon initialized; component readiness is reported through health diagnostics"
    );
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut next_audio_diagnostic = std::time::Instant::now();
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let persistent_audio_diagnostic = AudioStatusWriter::new(
        daemon
            .config()
            .state_dir
            .join("diagnostics/last-audio-session.json"),
    );
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut next_persistent_audio_diagnostic = std::time::Instant::now();
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut persisted_audio_generation = 0_u64;
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut persisted_audio_frames = 0_u64;
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    let mut logged_audio_status: Option<AudioRuntimeSnapshot> = None;
    let mut logged_failures = BTreeMap::<String, String>::new();
    loop {
        let snapshot = daemon.poll()?;
        let failures = snapshot
            .health
            .components()
            .iter()
            .filter(|(_, health)| health.required && health.state == HealthState::Failed)
            .map(|(component, health)| (component.clone(), health.detail.clone()))
            .collect::<BTreeMap<_, _>>();
        for (component, detail) in &failures {
            if logged_failures.get(component) != Some(detail) {
                warn!(component, detail, "required diagnostic failed");
            }
        }
        for component in logged_failures.keys() {
            if !failures.contains_key(component) {
                info!(component, "required diagnostic recovered");
            }
        }
        logged_failures = failures;
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let host_ui_events = host_ui
            .as_ref()
            .map_or_else(Vec::new, |host_ui| host_ui.take_events());
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        for event in host_ui_events {
            let event = match event {
                HostUiEvent::SettingsCommitted(event) => event,
                HostUiEvent::ConnectPhone(address) => {
                    if let Some(wireless) = &wireless {
                        connect_phone(&mut daemon, &wireless.directory(), &address);
                    }
                    continue;
                }
                HostUiEvent::DisconnectPhone(address) => {
                    if let Some(wireless) = &wireless {
                        disconnect_phone(&mut daemon, &wireless.directory(), &address);
                    }
                    continue;
                }
                HostUiEvent::ForgetPhone(address) => {
                    if let Some(wireless) = &wireless {
                        forget_phone(&mut daemon, &wireless.directory(), &address);
                    }
                    continue;
                }
            };
            if event.radio_changed
                && let Some(wireless) = &wireless
                && let Some((channel, width)) = event.settings.wifi_radio()
            {
                info!(
                    channel,
                    width_mhz = width.mhz(),
                    "applying the Wi-Fi settings from the host UI"
                );
                wireless.set_radio(channel, width);
            }
            if !event.restart_required {
                continue;
            }
            let mut candidate_config = settings_base_config.clone();
            if let Err(error) = event
                .settings
                .apply_to_config_preserving_equalizer(&mut candidate_config)
            {
                warn!(%error, "host settings could not be applied");
                continue;
            }
            let candidate_runtime = match SelectedRuntime::from_config_with_display_and_ambient(
                &candidate_config,
                display
                    .as_ref()
                    .map(prelude_media::LinuxDisplayRuntime::handle),
                ambient_light.clone(),
            ) {
                Ok(mut runtime) => {
                    if let Some(wireless) = &wireless {
                        runtime.attach_wireless(
                            &candidate_config,
                            wireless,
                            display
                                .as_ref()
                                .map(prelude_media::LinuxDisplayRuntime::handle),
                            ambient_light.clone(),
                        );
                    }
                    runtime
                }
                Err(error) => {
                    warn!(%error, "could not rebuild the projection runtime for the new settings");
                    continue;
                }
            };
            let was_wireless = daemon.has_wireless_phone();
            if let Err(error) =
                daemon.replace_projection_runtime(candidate_config, candidate_runtime)
            {
                warn!(%error, "could not restart the projection runtime after settings changed");
            } else {
                info!("restarted the projection runtime after settings changed");
                if let Some(wireless) = &wireless {
                    wireless.set_phone_audio(event.settings.phone_audio);
                    if was_wireless {
                        wireless.reconnect_last_phone();
                    }
                }
            }
        }

        match daemon.maintain_projection() {
            Ok(true) => info!("started bounded wired phone-projection session worker"),
            Ok(false) => {}
            Err(error) => debug!(%error, "wired carrier/iAP2 session is not ready"),
        }
        if let Some(wireless) = &wireless {
            wireless.set_usb_projection(daemon.projecting_over_usb());
        }
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            let now = std::time::Instant::now();
            if now >= next_audio_diagnostic {
                let status = daemon.audio_status_snapshot();
                if logged_audio_status.as_ref() != Some(&status) {
                    info!(status = ?status, "audio runtime diagnostic");
                    logged_audio_status = Some(status.clone());
                }
                next_audio_diagnostic = now + std::time::Duration::from_secs(2);
                if should_persist_audio_diagnostic(
                    &status,
                    persisted_audio_generation,
                    persisted_audio_frames,
                    now,
                    next_persistent_audio_diagnostic,
                ) {
                    match persistent_audio_diagnostic.write(&status) {
                        Ok(()) => {
                            persisted_audio_generation = status.pipeline_generation;
                            persisted_audio_frames = status.frames_accepted;
                            next_persistent_audio_diagnostic =
                                now + std::time::Duration::from_secs(60);
                        }
                        Err(error) => warn!(
                            %error,
                            "persisting the bounded audio session diagnostic failed"
                        ),
                    }
                }
            }
        }
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        let host_ui_wakeup = {
            let wake = host_ui_wake.clone();
            async move {
                if let Some(wake) = wake {
                    wake.notified().await;
                } else {
                    std::future::pending::<()>().await;
                }
            }
        };
        #[cfg(not(all(target_os = "linux", feature = "linux-media")))]
        let host_ui_wakeup = std::future::pending::<()>();
        tokio::select! {
            () = tokio::time::sleep(daemon.polling_interval()) => {}
            () = host_ui_wakeup => {}
            signal = &mut shutdown => {
                if let Err(error) = signal {
                    warn!(%error, "failed to wait for shutdown signal");
                }
                break;
            }
        }
    }
    if let Err(error) = daemon.stop() {
        warn!(%error, "failed to close projection and restore the prior USB state");
    }
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    drop(host_ui);
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    if let Some(display) = display {
        if let Err(error) = display.shutdown() {
            warn!(%error, "failed to stop the local display runtime");
        }
    }
    info!("projection diagnostics daemon stopped");
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn connect_phone(
    daemon: &mut Daemon<CombinedDeviceDiscovery, SelectedRuntime>,
    directory: &WirelessPhoneDirectory,
    address: &str,
) {
    if let Err(error) = daemon.choose_wireless() {
        warn!(%error, "could not stop the USB phone's session");
    }
    if let Some(active) = directory.active().filter(|active| active != address) {
        if let Err(error) = daemon.stop() {
            warn!(%error, "could not stop the current phone's session");
        }
        directory.disconnect(&active);
    }
    match directory.connect(address) {
        Ok(()) => info!(peer = address, "paging the phone chosen in settings"),
        Err(error) => warn!(peer = address, %error, "could not connect the chosen phone"),
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn disconnect_phone(
    daemon: &mut Daemon<CombinedDeviceDiscovery, SelectedRuntime>,
    directory: &WirelessPhoneDirectory,
    address: &str,
) {
    if directory.active().as_deref() == Some(address)
        && let Err(error) = daemon.stop()
    {
        warn!(%error, "could not stop the disconnected phone's session");
    }
    directory.dismiss(address);
    info!(peer = address, "disconnected the phone");
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn forget_phone(
    daemon: &mut Daemon<CombinedDeviceDiscovery, SelectedRuntime>,
    directory: &WirelessPhoneDirectory,
    address: &str,
) {
    if directory.active().as_deref() == Some(address)
        && let Err(error) = daemon.stop()
    {
        warn!(%error, "could not stop the forgotten phone's session");
    }
    match directory.forget(address) {
        Ok(()) => info!(peer = address, "forgot the phone"),
        Err(error) => warn!(peer = address, %error, "could not forget the phone"),
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn should_persist_audio_diagnostic(
    status: &AudioRuntimeSnapshot,
    persisted_audio_generation: u64,
    persisted_audio_frames: u64,
    now: std::time::Instant,
    next_allowed: std::time::Instant,
) -> bool {
    let new_pipeline = status.pipeline_generation != persisted_audio_generation;
    status.pipeline_state == AudioPipelineState::Playing
        && status.backend.is_some()
        && status.main_pcm_level_observations != 0
        && (new_pipeline
            || (status.frames_accepted > persisted_audio_frames && now >= next_allowed))
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
