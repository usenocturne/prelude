SUMMARY = "CT projection development image"
DESCRIPTION = "USB-recoverable wired and wireless CarPlay and Android Auto development image for a qualified charge-through OTG adapter, with boot-enabled gadget recovery and hardware diagnostics."
LICENSE = "GPL-3.0-only"

SUPERBIRD_ROOTFS_TYPE = "squashfs-lz4"
PRELUDE_FLAVOUR = "dev"

require prelude-image-base.inc

IMAGE_FEATURES += "tools-debug post-install-logging"

IMAGE_INSTALL:append = " \
    packagegroup-prelude-dev \
    prelude-topology-otg \
"

prelude_validate_otg_topology() {
    marker="${IMAGE_ROOTFS}${libexecdir}/prelude-topology-otg"
    if [ ! -f "$marker" ]; then
        bbfatal "OTG topology marker is missing"
    fi
    config="${IMAGE_ROOTFS}${sysconfdir}/prelude/config-dev.toml"
    daemon_dropin="${IMAGE_ROOTFS}${systemd_system_unitdir}/preluded.service.d/10-dev.conf"
    if [ ! -f "$config" ] || [ ! -f "$daemon_dropin" ]; then
        bbfatal "OTG topology does not install its live daemon configuration"
    fi
    if ! grep -qx 'ExecStart=/usr/bin/preluded --config /etc/prelude/config-dev.toml' "$daemon_dropin"; then
        bbfatal "OTG topology daemon does not select config-dev.toml"
    fi
    for setting in \
        'configuration_reconnect_timeout_ms = 15000' \
        'mux_io_timeout_ms = 5000' \
        'mode = "automatic"' \
        '[android_auto]' \
        'enable_live_session = true' \
        'disable_audio_output = false' \
        'video = "amlogic_v4l2"' \
        'display_width_physical_mm = 86' \
        'display_height_physical_mm = 52' \
        'display_max_fps = 60'; do
        if ! grep -Fqx "$setting" "$config"; then
            bbfatal "OTG topology configuration is missing: $setting"
        fi
    done
    for unit in \
        superbird-usb-gadget.service \
        preluded.service \
        prelude-usb-host.service \
        prelude-host-trial-next-boot.service \
        prelude-device-recovery.path \
        prelude-otg-recovery.service \
        prelude-otg-recovery.path; do
        wants="${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/$unit"
        if [ ! -L "$wants" ]; then
            bbfatal "OTG topology does not enable $unit"
        fi
    done
    for path in \
        "${libexecdir}/prelude-session-recover" \
        "${systemd_system_unitdir}/prelude-otg-recovery.service" \
        "${systemd_system_unitdir}/prelude-otg-recovery.path" \
        "${systemd_system_unitdir}/prelude-usb-host.service.d/20-otg-always-host.conf" \
        "${systemd_system_unitdir}/prelude-host-health.service.d/20-otg-always-host.conf" \
        "${systemd_system_unitdir}/prelude-host-health-failure.service.d/20-otg-always-host.conf"; do
        if [ ! -e "${IMAGE_ROOTFS}$path" ]; then
            bbfatal "OTG topology recovery payload is missing: $path"
        fi
    done
    host_policy="${IMAGE_ROOTFS}${systemd_system_unitdir}/prelude-usb-host.service.d/20-otg-always-host.conf"
    if ! grep -qx 'ExecStart=/usr/bin/preludectl automatic-host-boot' "$host_policy"; then
        bbfatal "OTG topology does not activate the guarded automatic-host boot policy"
    fi
    if [ "$(grep -c '^ConditionPathExists=$' "$host_policy")" -ne 1 ] || \
       ! grep -qx 'ConditionPathExists=/usr/libexec/prelude-topology-otg' "$host_policy"; then
        bbfatal "OTG automatic-host policy does not replace mutable confirmation gating"
    fi
    for path in \
        "${libexecdir}/prelude-usbip-client" \
        "${systemd_system_unitdir}/prelude-usbip-client.service" \
        "${sbindir}/usbip"; do
        if [ -e "${IMAGE_ROOTFS}$path" ]; then
            bbfatal "OTG image contains unsupported USB/IP payload: $path"
        fi
    done
}
ROOTFS_POSTPROCESS_COMMAND += "prelude_validate_otg_topology; "
