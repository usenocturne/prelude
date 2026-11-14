SUMMARY = "CT wired and wireless phone-projection daemon and control utility"
DESCRIPTION = "Automatic wired and wireless CarPlay and Android Auto runtimes plus Superbird USB role, health, recovery, and diagnostic control."
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

require recipes-core/prelude-monorepo.inc

SRC_URI += " \
    file://preluded.service \
    file://prelude-backlight.service \
    file://99-zz-prelude-backlight.rules \
    file://99-prelude-usb-power.rules \
    file://mt76_usb.conf \
    file://prelude-usb-host.service \
    file://prelude-host-trial-next-boot.service \
    file://prelude-host-health.service \
    file://prelude-host-health-failure.service \
    file://prelude-device-recovery.service \
    file://prelude-device-recovery.path \
    file://prelude-host-health.timer \
    file://preluded.env \
    file://preluded-dev.conf \
    file://preluded-wireless.conf \
    file://journald-persistent.conf \
    file://prelude.tmpfiles.conf \
    file://prelude-dev.tmpfiles.conf \
    file://config.toml \
    file://config-dev.toml \
    file://carplay-logo.nv12 \
    file://android-auto-logo.nv12 \
"
# ISO 3166-1 alpha-2 code for the access point's regulatory domain.
PRELUDE_WIRELESS_COUNTRY ??= "US"
# A fixed WPA2 passphrase lets a laptop join the access point and SSH to
# 10.47.0.1 while the Car Thing is in USB host mode. Unset generates one.
PRELUDE_WIRELESS_PASSPHRASE ??= ""

inherit cargo pkgconfig systemd

# Cargo.lock is authoritative. Network access remains enabled until every
# registry/git source is represented as a BitBake fetcher or vendored source.
do_compile[network] = "1"
CARGO_DISABLE_BITBAKE_VENDORING = "1"
CARGO_BUILD_FLAGS:remove = "--frozen"
CARGO_BUILD_FLAGS:append = " --locked --package preluded --package preludectl --features preluded/linux-media --config profile.release.strip=false"

# C dependencies compiled by Cargo (notably ring) are unpacked below WORKDIR,
# outside EXTERNALSRC and B. Remap that parent as well so their DWARF does not
# leak TMPDIR into the split debug package.
DEBUG_PREFIX_MAP:append = " -ffile-prefix-map=${WORKDIR}=${TARGET_DBGSRC_DIR}"

DEPENDS = " \
    gstreamer1.0 \
    gstreamer1.0-plugins-base \
    libusb1 \
    openssl \
"

SYSTEMD_SERVICE:${PN} = " \
    prelude-backlight.service \
    prelude-usb-host.service \
    prelude-host-trial-next-boot.service \
    prelude-host-health.service \
    prelude-host-health-failure.service \
    prelude-device-recovery.service \
    prelude-device-recovery.path \
    prelude-host-health.timer \
    preluded.service \
"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"
SYSTEMD_PACKAGES = "${PN}"

RDEPENDS:${PN} += " \
    superbird-usb-role \
"

do_install() {
    install -d ${D}${bindir}
    install -m 0755 ${B}/target/${CARGO_TARGET_SUBDIR}/preluded ${D}${bindir}/preluded
    install -m 0755 ${B}/target/${CARGO_TARGET_SUBDIR}/preludectl ${D}${bindir}/preludectl

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/preluded.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-backlight.service ${D}${systemd_system_unitdir}/
    # preluded owns the panel backlight. systemd-backlight would restore the
    # level saved at the last clean shutdown (often the dark-cabin floor).
    install -d ${D}${sysconfdir}/systemd/system
    ln -sf /dev/null ${D}${sysconfdir}/systemd/system/systemd-backlight@backlight:backlight.service
    # Sort after 99-systemd.rules so ALS brightness CHANGE uevents do not enqueue
    # the masked systemd-backlight unit (and fill the journal).
    install -d ${D}${sysconfdir}/udev/rules.d
    install -m 0644 ${UNPACKDIR}/99-zz-prelude-backlight.rules \
        ${D}${sysconfdir}/udev/rules.d/
    install -m 0644 ${UNPACKDIR}/99-prelude-usb-power.rules \
        ${D}${sysconfdir}/udev/rules.d/
    install -d ${D}${sysconfdir}/modprobe.d
    install -m 0644 ${UNPACKDIR}/mt76_usb.conf \
        ${D}${sysconfdir}/modprobe.d/
    install -m 0644 ${UNPACKDIR}/prelude-usb-host.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-host-trial-next-boot.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-host-health.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-host-health-failure.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-device-recovery.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-device-recovery.path ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-host-health.timer ${D}${systemd_system_unitdir}/
    install -d ${D}${sysconfdir}/default
    install -m 0644 ${UNPACKDIR}/preluded.env ${D}${sysconfdir}/default/preluded

    install -d ${D}${sysconfdir}/prelude
    install -m 0644 ${UNPACKDIR}/config.toml ${D}${sysconfdir}/prelude/config.toml
    install -m 0644 ${UNPACKDIR}/config-dev.toml ${D}${sysconfdir}/prelude/config-dev.toml

    # Status text is drawn by preluded at runtime; these frames only carry the
    # logos. Pin their hashes so a partial or mismatched update cannot ship.
    install -d ${D}${datadir}/prelude
    for overlay in carplay-logo.nv12 android-auto-logo.nv12; do
        if [ "$(wc -c < ${UNPACKDIR}/$overlay)" -ne 576000 ]; then
            bbfatal "$overlay is not an 800x480 NV12 frame"
        fi
        case "$overlay" in
            carplay-logo.nv12)
                expected="036c4d51198eaf0fbb2fd13c3d3b6cfab1eaff5b44ecf8c5d17ea5267075b124"
                ;;
            android-auto-logo.nv12)
                expected="5c658f778f8265a02913e6c77d684d542fe95951225f33e53019f3ae3ae3fb52"
                ;;
        esac
        if [ "$(sha256sum ${UNPACKDIR}/$overlay | cut -d' ' -f1)" != "$expected" ]; then
            bbfatal "$overlay does not match the supplied logo frame"
        fi
        install -m 0644 ${UNPACKDIR}/$overlay ${D}${datadir}/prelude/$overlay
    done

    install -d ${D}${nonarch_libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/prelude.tmpfiles.conf \
        ${D}${nonarch_libdir}/tmpfiles.d/prelude.conf
    # The development-only tmpfiles fragment provisions the pinned pair-setup
    # code after the writable /var partition is mounted. Production never
    # installs this fragment or a default setup code.
    install -m 0644 ${UNPACKDIR}/prelude-dev.tmpfiles.conf \
        ${D}${nonarch_libdir}/tmpfiles.d/prelude-dev.conf

    install -d ${D}${systemd_system_unitdir}/preluded.service.d
    install -m 0644 ${UNPACKDIR}/preluded-dev.conf \
        ${D}${systemd_system_unitdir}/preluded.service.d/10-dev.conf
    install -m 0644 ${UNPACKDIR}/preluded-wireless.conf \
        ${D}${systemd_system_unitdir}/preluded.service.d/30-wireless.conf
    # Wireless failures happen with the hub attached, and the journal is only
    # readable after rebooting into USB recovery mode.
    install -d ${D}${systemd_unitdir}/journald.conf.d
    install -m 0644 ${UNPACKDIR}/journald-persistent.conf \
        ${D}${systemd_unitdir}/journald.conf.d/20-prelude-persistent.conf

    country="${PRELUDE_WIRELESS_COUNTRY}"
    case "$country" in
        [A-Z][A-Z]) ;;
        *) bbfatal "PRELUDE_WIRELESS_COUNTRY must be an ISO 3166-1 alpha-2 code" ;;
    esac
    printf '\n[wireless]\nenabled = true\ncountry = "%s"\n' "$country" \
        >> ${D}${sysconfdir}/prelude/config-dev.toml

    passphrase="${PRELUDE_WIRELESS_PASSPHRASE}"
    if [ -n "$passphrase" ]; then
        if [ "${#passphrase}" -lt 8 ] || [ "${#passphrase}" -gt 63 ]; then
            bbfatal "PRELUDE_WIRELESS_PASSPHRASE must be 8-63 characters"
        fi
        case "$passphrase" in
            *[!A-Za-z0-9._~-]*) bbfatal "PRELUDE_WIRELESS_PASSPHRASE may only contain A-Z a-z 0-9 . _ ~ -" ;;
        esac
        printf 'passphrase = "%s"\n' "$passphrase" >> ${D}${sysconfdir}/prelude/config-dev.toml
    fi
}

PACKAGES =+ "${PN}-dev-config"

FILES:${PN}-dev-config = " \
    ${systemd_system_unitdir}/preluded.service.d/10-dev.conf \
    ${sysconfdir}/prelude/config-dev.toml \
    ${nonarch_libdir}/tmpfiles.d/prelude-dev.conf \
"
RDEPENDS:${PN}-dev-config = "${PN}"
SUMMARY:${PN}-dev-config = "Experimental projection configuration and provisioning for Prelude development images"

# Cargo leaves symbols intact so Yocto can split debug data and strip binaries
# according to the image policy. The tmpfiles fragment is runtime configuration.
FILES:${PN}:append = " \
    ${sysconfdir}/systemd/system/systemd-backlight@backlight:backlight.service \
    ${sysconfdir}/udev/rules.d/99-zz-prelude-backlight.rules \
    ${nonarch_libdir}/tmpfiles.d/prelude.conf \
    ${datadir}/prelude/*.nv12 \
    ${systemd_system_unitdir}/preluded.service.d/30-wireless.conf \
    ${systemd_unitdir}/journald.conf.d/20-prelude-persistent.conf \
"

# externalsrc normally clones .git/index to hash local changes. A freshly
# initialized greenfield repository has no index yet, so hash the actual Cargo
# workspace inputs directly. This also avoids invalidating the Rust build when
# Yocto's image/build or image/sources trees change.
python () {
    if d.getVar("EXTERNALSRC"):
        source = d.getVar("EXTERNALSRC")
        checksums = " ".join((
            f"{source}/Cargo.toml:True",
            f"{source}/Cargo.lock:True",
            f"{source}/crates/*:True",
        ))
        d.setVarFlag("do_configure", "file-checksums", checksums)
        d.setVarFlag("do_compile", "file-checksums", checksums)
}
