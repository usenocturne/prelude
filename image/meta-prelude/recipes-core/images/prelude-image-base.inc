inherit core-image
inherit superbird-headroom-check
inherit superbird-image
inherit mainline-flashthing

IMAGE_FEATURES += "ssh-server-openssh post-install-logging"

IMAGE_INSTALL = " \
    packagegroup-core-boot \
    packagegroup-prelude-core \
"

# The BSP's weston.ini loads this module unconditionally. Keep the package and
# configuration coupled so a missing module fails at image construction time.
prelude_validate_weston_modules() {
    for module in cursor-suppress.so systemd-notify.so; do
        if [ ! -e "${IMAGE_ROOTFS}${libdir}/weston/$module" ]; then
            bbfatal "weston.ini requires ${libdir}/weston/$module"
        fi
    done
}
ROOTFS_POSTPROCESS_COMMAND += "prelude_validate_weston_modules; "

# OTA: record which bundle flavour this image takes, and disable updates on
# images built without a signing public key (or images that must never update).
PRELUDE_FLAVOUR ??= "prod"
PRELUDE_OTA_ENABLED ??= "1"
PRELUDE_OTA_PUBLIC_KEY ??= ""
prelude_write_ota_markers() {
    case "${PRELUDE_FLAVOUR}" in
        prod|dev) ;;
        *) bbfatal "PRELUDE_FLAVOUR must be prod or dev" ;;
    esac
    install -d "${IMAGE_ROOTFS}${sysconfdir}/prelude"
    printf '%s\n' "${PRELUDE_FLAVOUR}" > "${IMAGE_ROOTFS}${sysconfdir}/prelude/flavour"
    rm -f "${IMAGE_ROOTFS}${sysconfdir}/prelude/ota-disabled"
    if [ "${PRELUDE_OTA_ENABLED}" != "1" ] || [ -z "${PRELUDE_OTA_PUBLIC_KEY}" ]; then
        : > "${IMAGE_ROOTFS}${sysconfdir}/prelude/ota-disabled"
    fi
}
ROOTFS_POSTPROCESS_COMMAND += "prelude_write_ota_markers; "

# A scheduled trial must be consumed automatically on the next boot. Keep the
# unit payload and its boot enablement coupled to every image variant.
prelude_validate_host_trial_boot_unit() {
    unit="${IMAGE_ROOTFS}${systemd_system_unitdir}/prelude-host-trial-next-boot.service"
    wants="${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/prelude-host-trial-next-boot.service"
    if [ ! -f "$unit" ]; then
        bbfatal "next-boot host-trial consumer unit is missing"
    fi
    if [ ! -L "$wants" ]; then
        bbfatal "next-boot host-trial consumer unit is not boot-enabled"
    fi
}
ROOTFS_POSTPROCESS_COMMAND += "prelude_validate_host_trial_boot_unit; "

# BlueALSA is a direct dependency of the pinned BSP's generic runtime
# packagegroup. preluded owns the Bluetooth profiles it needs, so excluding it
# turns an accidental reintroduction into a rootfs construction failure.
PACKAGE_EXCLUDE += "bluealsa"

# Wireless projection needs the BSP Bluetooth bring-up, BlueZ, and the access
# point tooling that preluded spawns.
prelude_validate_wireless_rootfs() {
    for path in \
        ${libexecdir}/bluetooth/bluetoothd \
        ${bindir}/superbird-bluetooth \
        ${bindir}/hcitool \
        ${nonarch_base_libdir}/firmware/brcm/BCM20703A2.hcd \
        ${nonarch_base_libdir}/firmware/regulatory.db \
        ${systemd_system_unitdir}/bluetooth.service \
        ${systemd_system_unitdir}/superbird-bluetooth.service \
        ${systemd_system_unitdir}/preluded.service.d/30-wireless.conf \
        ${systemd_unitdir}/journald.conf.d/20-prelude-persistent.conf \
        ${sbindir}/hostapd \
        ${bindir}/dnsmasq \
        ${sbindir}/iw; do
        if [ ! -e "${IMAGE_ROOTFS}$path" ]; then
            bbfatal "wireless image is missing $path"
        fi
    done
    if [ -e "${IMAGE_ROOTFS}${bindir}/bluealsa" ]; then
        bbfatal "wireless image unexpectedly contains ${bindir}/bluealsa"
    fi
    for unit in dnsmasq.service hostapd.service; do
        if [ -L "${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/$unit" ]; then
            bbfatal "wireless image enables $unit, which conflicts with preluded"
        fi
    done
}
ROOTFS_POSTPROCESS_COMMAND += "prelude_validate_wireless_rootfs; "

# Kernel modules are selected individually by packagegroup-prelude-core.
BAD_RECOMMENDATIONS += "kernel-modules udev-hwdb wpa-supplicant wireless-regdb wireless-regdb-static weston-init"

SUPERBIRD_ROOTFS_TYPE ??= "ext4"
EXTRA_IMAGECMD:squashfs-lz4 = "-b 1M -no-xattrs -all-root -Xhc"
EXTRA_IMAGECMD:ext4 = "-i 16384 -m 0 -O ^has_journal -O ^huge_file"

IMAGE_FSTYPES = "wic ${SUPERBIRD_ROOTFS_TYPE}"
WKS_FILE = "superbird-mainline.wks.in"

do_image_wic[depends] += " \
    virtual/kernel:do_deploy \
    superbird-uenv:do_deploy \
    superbird-extlinux:do_deploy \
    superbird-logo:do_deploy \
"

EXTRA_IMAGEDEPENDS += "superbird-uboot"
