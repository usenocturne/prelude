# Install the OTA verification key when the build provides one. Without it the
# image is built with OTA disabled (/etc/prelude/ota-disabled, see
# prelude-image-base.inc) and swupdate cannot accept any bundle.
PRELUDE_OTA_PUBLIC_KEY ??= ""

do_install[vardeps] += "PRELUDE_OTA_PUBLIC_KEY"
do_install[file-checksums] += "${@'%s:True' % d.getVar('PRELUDE_OTA_PUBLIC_KEY') if d.getVar('PRELUDE_OTA_PUBLIC_KEY') else ''}"

do_install:append() {
    key="${PRELUDE_OTA_PUBLIC_KEY}"
    if [ -n "$key" ]; then
        if [ ! -f "$key" ]; then
            bbfatal "PRELUDE_OTA_PUBLIC_KEY does not name a readable file: $key"
        fi
        install -d ${D}${sysconfdir}/swupdate
        install -m 0644 "$key" ${D}${sysconfdir}/swupdate/public.pem
        sed -i 's|public-key-file = "";|public-key-file = "/etc/swupdate/public.pem";|' \
            ${D}${sysconfdir}/swupdate.cfg
        if ! grep -q 'public-key-file = "/etc/swupdate/public.pem";' ${D}${sysconfdir}/swupdate.cfg; then
            bbfatal "could not point swupdate.cfg at the OTA public key"
        fi
    fi
}

FILES:${PN} += "${sysconfdir}/swupdate/public.pem"
