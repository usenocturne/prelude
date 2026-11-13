SUMMARY = "Prelude development A/B update bundle"

require recipes-core/superbird-bsp-update/superbird-bsp-update.inc
require prelude-update-signing.inc

PRELUDE_UPDATE_FLAVOUR = "dev"
SUPERBIRD_OTA_SOURCE_IMAGE = "prelude-dev-image"
SUPERBIRD_OTA_SYSTEM_ARTIFACT = "${SUPERBIRD_OTA_SOURCE_LINKNAME}.squashfs-lz4"
SUPERBIRD_OTA_SYSTEM_CPIO_NAME = "system.img"
