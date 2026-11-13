SUMMARY = "Prelude visible USB-host qualification image"
DESCRIPTION = "USB-recoverable development image that performs one bounded host-mode enumeration test on every boot and displays PASS or FAIL on-panel."
LICENSE = "GPL-3.0-only"

SUPERBIRD_ROOTFS_TYPE = "squashfs-lz4"
# Adapter qualification image: never offer or accept OTA updates.
PRELUDE_FLAVOUR = "dev"
PRELUDE_OTA_ENABLED = "0"

require prelude-image-base.inc

IMAGE_FEATURES += "tools-debug post-install-logging"

IMAGE_INSTALL:append = " \
    packagegroup-prelude-dev \
    prelude-host-test \
"

# Qualification owns the USB role and display. Keep projection installed for
# diagnostics but do not auto-start it.
prelude_apply_host_test_policy() {
    rm -f "${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/preluded.service"

    for unit in prelude-host-test.service prelude-host-test-display.service; do
        if [ ! -f "${IMAGE_ROOTFS}${systemd_system_unitdir}/$unit" ]; then
            bbfatal "USB host-test unit is missing: $unit"
        fi
        if [ ! -L "${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/$unit" ]; then
            bbfatal "USB host-test unit is not enabled: $unit"
        fi
    done

    unit=preluded.service
    if [ -e "${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/$unit" ]; then
        bbfatal "USB host-test image unexpectedly enables $unit"
    fi
    if [ ! -f "${IMAGE_ROOTFS}${systemd_system_unitdir}/$unit.d/host-test.conf" ]; then
        bbfatal "USB host-test image does not inhibit $unit"
    fi
}
ROOTFS_POSTPROCESS_COMMAND += "prelude_apply_host_test_policy; "
