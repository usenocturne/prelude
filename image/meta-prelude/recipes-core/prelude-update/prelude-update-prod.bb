SUMMARY = "Prelude production A/B update bundle"

require recipes-core/superbird-bsp-update/superbird-bsp-update.inc
require prelude-update-signing.inc

PRELUDE_UPDATE_FLAVOUR = "prod"
SUPERBIRD_OTA_SOURCE_IMAGE = "prelude-prod-image"
SUPERBIRD_OTA_SYSTEM_ARTIFACT = "${SUPERBIRD_OTA_SOURCE_LINKNAME}.ext4"
SUPERBIRD_OTA_SYSTEM_CPIO_NAME = "system.img"
