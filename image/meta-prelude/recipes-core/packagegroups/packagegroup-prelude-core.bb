SUMMARY = "CT phone-projection core runtime"
DESCRIPTION = "CarPlay and Android Auto services, USB host classes, audio transports, and display runtime on the Superbird BSP."
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

PACKAGE_ARCH = "${MACHINE_ARCH}"

inherit packagegroup

PACKAGES = "${PN}"

RDEPENDS:${PN} = " \
    packagegroup-superbird-runtime \
    prelude-runtime \
    \
    alsa-utils \
    coreutils \
    iproute2 \
    libusb1 \
    \
    mesa \
    libdrm \
    weston \
    cursor-suppress \
    prelude-inter-font \
    ttf-roboto \
    gstreamer1.0 \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-base-alsa \
    gstreamer1.0-plugins-base-app \
    gstreamer1.0-plugins-base-audioconvert \
    gstreamer1.0-plugins-base-audiomixer \
    gstreamer1.0-plugins-base-audioresample \
    gstreamer1.0-plugins-base-audiotestsrc \
    gstreamer1.0-plugins-base-opus \
    gstreamer1.0-plugins-base-volume \
    gstreamer1.0-plugins-base-videoconvertscale \
    gstreamer1.0-plugins-good-audioparsers \
    gstreamer1.0-plugins-good-equalizer \
    gstreamer1.0-plugins-good-rtp \
    gstreamer1.0-plugins-good-rtpmanager \
    gstreamer1.0-plugins-good-video4linux2 \
    gstreamer1.0-plugins-bad-faad \
    gstreamer1.0-plugins-bad-videoparsersbad \
    \
    kernel-module-snd-usb-audio \
    kernel-module-usb-storage \
    kernel-module-uas \
    kernel-module-usbnet \
    kernel-module-cdc-ether \
    kernel-module-cdc-ncm \
    kernel-module-ipheth \
    kernel-module-tun \
    kernel-module-ax88179-178a \
    kernel-module-r8152 \
    kernel-module-cdc-acm \
    kernel-module-usbmon \
"

# Wireless projection: onboard BCM20703A2 Bluetooth carries the bootstrap and a
# USB Wi-Fi adapter hosts the access point. Every mainline USB driver with AP
# support is installed so any compatible adapter works without image changes.
# mac80211 allocates ccm(aes)/gcm(aes) for every key and requests them by
# module alias, so the generic fallbacks for the built-in ARMv8 CE ciphers are
# listed explicitly. wpa-supplicant lets the same adapter join a home network
# or phone hotspot to get online; it stays in BAD_RECOMMENDATIONS so nothing
# else pulls it in.
RDEPENDS:${PN} += " \
    hostapd \
    dnsmasq \
    wpa-supplicant \
    iw \
    wireless-regdb-static \
    linux-firmware-prelude-mt76-usb \
    linux-firmware-mt7601u \
    linux-firmware-mt76x2 \
    linux-firmware-rtl8188 \
    linux-firmware-rtl8192 \
    linux-firmware-rtl8192cu \
    linux-firmware-rtl8723 \
    linux-firmware-rtl8812 \
    linux-firmware-rtl8814 \
    linux-firmware-rtl8821 \
    linux-firmware-rtl8822 \
    linux-firmware-rtl8851 \
    linux-firmware-rtl8852 \
    linux-firmware-ath9k \
    linux-firmware-carl9170 \
    linux-firmware-ralink \
    kernel-module-mt7601u \
    kernel-module-mt76x0u \
    kernel-module-mt76x2u \
    kernel-module-mt7663u \
    kernel-module-mt7921u \
    kernel-module-mt7925u \
    kernel-module-rtl8xxxu \
    kernel-module-rtw88-8723du \
    kernel-module-rtw88-8812au \
    kernel-module-rtw88-8814au \
    kernel-module-rtw88-8821au \
    kernel-module-rtw88-8821cu \
    kernel-module-rtw88-8822bu \
    kernel-module-rtw88-8822cu \
    kernel-module-rtw89-8851bu \
    kernel-module-rtw89-8852au \
    kernel-module-rtw89-8852bu \
    kernel-module-rtw89-8852cu \
    kernel-module-ath9k-htc \
    kernel-module-carl9170 \
    kernel-module-rt2800usb \
    kernel-module-aes \
    kernel-module-ccm \
    kernel-module-ctr \
    kernel-module-gcm \
    kernel-module-ghash-generic \
"
