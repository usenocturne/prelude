# OTA validation on hardware, 2026-11-13

Branch `feat/ota-image`, Plan 1 Task 6. One contributor Car Thing (superbird),
kernel 7.0.2-superbird, SWUpdate v2025.12, flashed with Terbium.

## Setup

| Artifact | Contents |
|---|---|
| Base image | `prelude-prod-image` 1.0.4, signed with a test key, `PRELUDE_WIRELESS_COUNTRY=CA`, fixed test passphrase |
| Bundle B | `prelude-update-prod` 1.0.5-test, test key, default US, no passphrase |
| Bundle C | `prelude-update-prod` 1.0.6-test, test key, `preluded` exits at startup (local patch, not committed) |
| D1-D4 | Bundle B repacked with `cpio -o -H crc`: no signature; signed with another key; one byte of `system.img` flipped; `sw-description` edited |

Access was over ADB in Back-held USB recovery. The Wi-Fi adapter on hand (Realtek,
2.4 GHz host only) did not pass client traffic on the access point, so SSH over
Wi-Fi was not used. Device logs and the swupdate output for every install are
kept with the contributor.

Normal boots and recovery boots were told apart in the journal by the
`automatic USB host mode bypassed for this boot because Back was held` line.
The journal syncs every five minutes by default, so a power pull loses the tail
of a short boot. For the health-gate checks the try counter was read within the
first 30 seconds of the following recovery boot, before that boot's own ready
marker reset it.

## Desk check (Step 0)

- The raw handler fsyncs each image (`handlers/raw_handler.c`) before the
  installer updates the boot environment (`core/installer.c`).
- The U-Boot environment is a single `uboot.env` file on FAT
  (`CONFIG_ENV_IS_IN_FAT`), not redundant. A power cut while it is written can
  corrupt it. The BSP already rewrites it on every boot; an update adds one
  write. Raised for upstream rather than blocking.

## Results

| Step | Result | Evidence |
|---|---|---|
| 1 Happy path | Pass | 1.0.5-test installed to slot b, booted b. `preluded is ready` at 53 s monotonic, `slot b healthy, reset slot_b_tries=3` in a normal (host-mode) boot; pending marker removed; `/etc/os-release` 1.0.5-test. |
| 2 Signature rejection | Pass | Unsigned: `description file name not the first of the list ... sw-description.sig`. Other key: `EVP_DigestVerifyFinal failed`. Image altered: signature `Verified OK`, then `HASH mismatch`. Description altered: `EVP_DigestVerifyFinal failed`. `slot_active` unchanged each time. |
| 3 Broken daemon fallback | Pass | Two normal boots of 1.0.6-test on slot a: watchdog at 300 s logged `slot a is on its update trial and preluded never became ready; rebooting`. U-Boot then booted b (`slot_active=b`, `slot_a_tries=0`); b reached ready and was marked healthy; pending marker removed. |
| 4 Recovery not looped | Pass | 1.0.6-test booted with Back held: `/run/prelude/recovery-boot` present, watchdog logged `this boot stayed in USB recovery; leaving it alone`, no reboot after 330 s, ADB usable. |
| 5 Identity preserved | Pass | After moving from the CA build to the US-default bundle, `wireless-identity.toml` kept `country = "CA"` and the passphrase while `/etc/prelude/config-dev.toml` said `US`. Phone reconnect confirmed on 2026-11-14 (below). |
| 6a Power cut during image write | Pass | Cut while `system.img` was streaming to root_b (log ends at `Installing STREAM system.img`). Booted slot a, `slot_active=a` unchanged, a marked healthy, stale pending marker removed. |
| 6b Power cut at boot-environment commit | Not run | The window is a single small FAT write and cannot be hit by hand. Covered by the desk check above. |
| 6c Power cut after install, before reboot | Pass | Install finished (34 s), power pulled, cold boot came up on the new slot a, reached ready and was marked healthy; pending marker removed. |

## Findings for upstream

1. The U-Boot environment is not redundant (desk check). A redundant
   environment, or two copies on the FAT partition, would make the slot switch
   power-cut safe.
2. A slot is marked good about 53 s after power-on. Three boots in a row that
   are shorter than that exhaust the try counter and U-Boot switches slots.
   This is not new: the BSP's own timer waited 60 s. A car that is started and
   stopped quickly three times would hit it.
3. A rejected or interrupted install leaves the inactive slot half written
   (SWUpdate streams images before the final hash check). The running slot is
   untouched, but there is no fallback until the next good install. The
   updater must not treat the inactive slot as a fallback after a failed
   install.
4. The image has no `swupdate-client`; the README now documents installing
   with `swupdate -i` after stopping the daemon. `adb reboot` is ignored by
   this adbd; use `adb shell systemctl reboot`.
5. Unrelated to OTA: `/var/lib/prelude` accumulates `.daemon-heartbeat.*.tmp`
   files across boots.

## Retest, 2026-11-14

A TP-Link Nano AC600 (RTL8811AU, `rtw88_8821au`) replaced the 2.4 GHz-only
adapter. It hosts the access point on both 2.4 and 5 GHz; DHCP and root SSH
over Wi-Fi worked, so this round ran entirely over Wi-Fi.

Bundle E, `prelude-update-prod` 1.0.7-test, adds the final-review fixes:
non-trial boots are marked good after 60 s, the persisted wireless identity is
validated, and swupdate is masked on keyless images.

| Check | Result | Evidence |
|---|---|---|
| OTA over Wi-Fi | Pass | Copied with `scp`, installed to slot b with `swupdate -i`, `slot_active=b`, `slot_b_tries=3`. |
| Trial boot gated on readiness | Pass | `preluded is ready` at 53 s, `slot b healthy, reset slot_b_tries=3`, pending marker removed. The 60 s time unit ran afterwards and changed nothing. |
| Boot without preluded marked good by time | Pass | `preluded.service` masked on slot b and `slot_b_tries=2`. No preluded output in that boot; at 62 s `prelude-slot-ok-time.service` ran and logged `slot b healthy, reset slot_b_tries=3`. Before the fix this boot was never marked good. |
| Fallback from a slot without preluded | Pass | preluded hosts the access point and UI, so that boot had no Wi-Fi. Three short boots exhausted `slot_b_tries` and U-Boot returned to slot a (1.0.5-test). preluded was unmasked on root_b from slot a. |
| Phone reconnect after OTA | Pass | On 1.0.7-test, slot b, the paired phone reconnected without re-pairing. Pairing records and the CA identity carried over. |
