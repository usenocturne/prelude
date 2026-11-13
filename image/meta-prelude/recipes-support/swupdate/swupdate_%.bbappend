# Prelude only accepts RSA-signed OTA bundles (see prelude-update-signing.inc).
# The BSP already enables CONFIG_HASH_VERIFY, which checks each image against
# the sha256 in the signed sw-description.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += "file://prelude-signing.cfg"
