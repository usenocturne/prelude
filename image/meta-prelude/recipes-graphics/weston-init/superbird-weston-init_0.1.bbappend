FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

# Use the same CarPlay mark for the later Weston splash instead of the
# upstream Superbird splash that appears after the bootloader handoff.
SUPERBIRD_WESTON_SPLASH_IMAGE = "carplay.png"

SRC_URI += " \
    file://weston-notify.conf \
    file://20-priority.conf \
"

# Weston signals readiness only after wayland-1 is listening, so units ordered
# after superbird-weston.service can connect immediately instead of racing it.
do_install:append() {
    for ini in weston-dev.ini weston-kiosk.ini; do
        sed -i 's/^modules=cursor-suppress\.so$/modules=cursor-suppress.so,systemd-notify.so/' \
            ${D}${sysconfdir}/$ini
        grep -qx 'modules=cursor-suppress.so,systemd-notify.so' ${D}${sysconfdir}/$ini
    done
    install -d ${D}${systemd_system_unitdir}/superbird-weston.service.d
    install -m 0644 ${UNPACKDIR}/weston-notify.conf \
        ${D}${systemd_system_unitdir}/superbird-weston.service.d/10-prelude-notify.conf
    install -m 0644 ${UNPACKDIR}/20-priority.conf \
        ${D}${systemd_system_unitdir}/superbird-weston.service.d/20-priority.conf
}

FILES:${PN} += " \
    ${systemd_system_unitdir}/superbird-weston.service.d/10-prelude-notify.conf \
    ${systemd_system_unitdir}/superbird-weston.service.d/20-priority.conf \
"
