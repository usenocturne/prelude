# Shared policy for Prelude OTA bundles: every bundle is RSA-signed, carries
# per-image SHA-256 hashes in its signed sw-description, is named for its
# flavour and version, and must fit the root partition it is written to.
#
# The private key is a path visible to BitBake (inside the kas container use
# /keys/..., see image/README.md). Images built without the matching public key
# ship with OTA disabled, so ordinary keyless image builds keep working.

# Prefer this layer's sw-description (adds sha256 lines) over the BSP copy.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

PRELUDE_OTA_SIGNING_KEY ??= ""
SWUPDATE_SIGNING = "RSA"
SWUPDATE_PRIVATE_KEY = "${PRELUDE_OTA_SIGNING_KEY}"

PRELUDE_UPDATE_FLAVOUR ??= ""
IMAGE_LINK_NAME = "prelude_update_${PRELUDE_UPDATE_FLAVOUR}_v${DISTRO_VERSION}"

# Task-time checks only: anonymous Python would run while parsing every recipe
# and break keyless image builds.
python prelude_require_signing_key() {
    import os
    flavour = d.getVar('PRELUDE_UPDATE_FLAVOUR')
    if flavour not in ('prod', 'dev'):
        bb.fatal("PRELUDE_UPDATE_FLAVOUR must be prod or dev, not %r" % flavour)
    key = d.getVar('PRELUDE_OTA_SIGNING_KEY')
    if not key:
        bb.fatal("Prelude OTA bundles must be signed: set PRELUDE_OTA_SIGNING_KEY "
                 "(and PRELUDE_OTA_KEY_DIR for the kas container). See image/README.md.")
    if not os.path.isfile(key):
        bb.fatal("PRELUDE_OTA_SIGNING_KEY does not name a readable file inside the "
                 "build environment: %s" % key)
}

python prelude_check_root_fits() {
    import os
    artifact = os.path.join(d.getVar('DEPLOY_DIR_IMAGE'),
                            d.getVar('SUPERBIRD_OTA_SYSTEM_ARTIFACT'))
    limit = int(d.getVar('SUPERBIRD_ROOT_PART_SIZE')) * 1024 * 1024
    size = os.path.getsize(artifact)
    if size > limit:
        bb.fatal("%s is %d bytes but root partitions are %d bytes"
                 % (os.path.basename(artifact), size, limit))
}

do_swuimage[prefuncs] += "prelude_require_signing_key prelude_check_root_fits"
do_swuimage[vardeps] += "PRELUDE_OTA_SIGNING_KEY PRELUDE_UPDATE_FLAVOUR"
