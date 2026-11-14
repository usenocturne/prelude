FILESEXTRAPATHS:prepend := "${THISDIR}/linux-superbird:"

SRC_URI:append = " \
    file://prelude-usb.cfg \
    file://prelude-wireless.cfg \
    file://prelude-crash-recovery.cfg \
    file://prelude-ramoops.dtsi \
"

# The BSP reinstalls its DTS on every configure, so appending here stays
# idempotent across reruns.
do_configure:append() {
    cat ${UNPACKDIR}/prelude-ramoops.dtsi >> \
        ${S}/arch/arm64/boot/dts/amlogic/meson-g12a-superbird.dts
}
