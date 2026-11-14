# The BSP marks the running slot good 60 s after every boot, whether or not
# Prelude works. An OTA image whose daemon crash-loops would then be kept
# forever. On an OTA trial boot (pending-update names the running slot) mark the
# slot good only once preluded reports ready, and reboot a trial that never gets
# there so U-Boot can fall back. Every other boot is still marked good after
# 60 s, or earlier once preluded is ready.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += " \
    file://prelude-slot-ok.path \
    file://prelude-slot-ok.conf \
    file://prelude-trial-watchdog \
    file://prelude-trial-watchdog.service \
    file://prelude-trial-watchdog.timer \
    file://prelude-slot-ok-time.service \
    file://prelude-slot-ok-time.timer \
"

SYSTEMD_SERVICE:${PN} = "prelude-slot-ok.path prelude-trial-watchdog.timer prelude-slot-ok-time.timer"

do_install:append() {
    install -m 0755 ${UNPACKDIR}/prelude-trial-watchdog ${D}${libexecdir}/prelude-trial-watchdog

    install -m 0644 ${UNPACKDIR}/prelude-slot-ok.path ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-trial-watchdog.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-trial-watchdog.timer ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-slot-ok-time.service ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-slot-ok-time.timer ${D}${systemd_system_unitdir}/

    install -d ${D}${systemd_system_unitdir}/superbird-slot-ok.service.d
    install -m 0644 ${UNPACKDIR}/prelude-slot-ok.conf \
        ${D}${systemd_system_unitdir}/superbird-slot-ok.service.d/10-prelude-ready.conf

    # Mask the BSP timer; prelude-slot-ok-time.timer replaces it and skips
    # OTA trial boots.
    install -d ${D}${sysconfdir}/systemd/system
    ln -sf /dev/null ${D}${sysconfdir}/systemd/system/superbird-slot-ok.timer
}

FILES:${PN} += " \
    ${libexecdir}/prelude-trial-watchdog \
    ${systemd_system_unitdir}/prelude-slot-ok.path \
    ${systemd_system_unitdir}/prelude-trial-watchdog.service \
    ${systemd_system_unitdir}/prelude-trial-watchdog.timer \
    ${systemd_system_unitdir}/prelude-slot-ok-time.service \
    ${systemd_system_unitdir}/prelude-slot-ok-time.timer \
    ${systemd_system_unitdir}/superbird-slot-ok.service.d/10-prelude-ready.conf \
    ${sysconfdir}/systemd/system/superbird-slot-ok.timer \
"
