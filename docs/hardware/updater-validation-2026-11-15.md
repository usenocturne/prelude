# Updater validation on hardware, 2026-11-14 to 2026-11-15

Branch `feat/ota-updater`, Plan 3 Task 7. One contributor Car Thing
(superbird), same TP-Link AC600 Nano Wi-Fi adapter as
`wifi-client-validation-2026-11-14.md`, joined to a home WPA2 router (the
saved network). Android Auto ran over USB between steps.

## Setup

Test releases were published on the contributor's fork (`weazil/prelude`)
and deleted afterwards. Each holds one `prelude_update_prod_v<version>.swu`
built from this branch with the test signing key, and the image points the
updater at the fork through `PRELUDE_UPDATES_REPOSITORY`. GitHub's asset
`digest` matched the local `sha256sum` of every upload.

| Release | Kind | Purpose |
|---|---|---|
| v1.0.19-test | release | older version, for rollback |
| v1.0.21-test | release | newest Stable |
| v1.0.22-test | pre-release | newest Beta |
| v1.0.23-test | release | tampered: one byte of `system.img` flipped |

The starting image (1.0.20-test, later 1.0.21/1.0.22-test) was installed
over the access point with SSH and `swupdate`. The bundles were rebuilt
after each fix below; the final round is at commit `60db7b2`, on top of
upstream `bc140af`.

## Results

| Step | Check | Result |
|---|---|---|
| 1 | Settings > Version opens Updates | Pass |
| 2 | Stable check through the saved network finds 1.0.21-test | Pass (after fix 1) |
| 3 | Install, Restart now, boots 1.0.21-test on the other slot; trial boot marked good, marker cleared, preluded `NRestarts=0` | Pass |
| 4 | Beta check finds 1.0.22-test; installs and boots | Pass |
| 5 | All versions lists Installed and "Older - rollback"; installing 1.0.19-test from 1.0.21-test works | Pass |
| 6 | Right after a restart the page shows "Finishing update…", then clears once the boot is marked good | Pass (after fix 2) |
| 7 | Cancel during the download returns to Updates; nothing written | Pass (after fix 5) |
| 8 | Wi-Fi adapter pulled mid-download gives "The download failed" | Pass after fix 5: about 5-10 s |
| 9 | Tampered 1.0.23-test: downloads and verifies (digest and version match the tampered asset), SWUpdate rejects it ("Image invalid or corrupted"), page shows "Install failed. Nothing was changed."; still on the old slot, no marker, no leftover download | Pass |

Not run: the phone hotspot as the update network (same code path as the
saved network, covered in the Wi-Fi client validation), and an image built
without an OTA key (covered by unit tests).

## Findings fixed during the run

1. Every real bundle was rejected as "The download is not the expected file"
   after a good download. SWUpdate writes newc cpio with the CRC magic
   `070702`; the version reader only accepted `070701` (test
   `bundle_version_reads_crc_format_archives`).
2. Restart now gave no feedback, and after the restart the trial boot showed
   "Installed - restart to finish" until the boot was marked good. The page
   now shows Restarting, and a marker for the running slot reads as
   "Finishing update…" and blocks installs as not yet healthy. The status
   moved under Check for updates so it stays in view when scrolled (tests
   `restart_shows_restarting_before_reboot`,
   `restart_during_trial_boot_does_nothing`,
   `pending_update_compares_marker_slot_with_running_slot`,
   `trial_boot_shows_finishing_instead_of_restart`,
   `check_status_shows_under_the_check_row`).
3. A found update's Install row and Restart now were added below the fold.
   They now take Check for updates' place (test
   `found_update_and_restart_take_the_check_row`).
4. Verifying, Installing and Restarting have no buttons, and a section with
   no rows is not drawn, so the page went blank. These steps now show a
   centred message and hide the back arrow (test
   `progress_without_buttons_still_says_what_is_happening`).
5. Pulling the adapter left the download stuck at 3% with Cancel and Back
   apparently dead: each read could wait out the whole body budget, and
   Cancel is only checked between reads. ureq 3.5's per-read timeout now
   caps any read at 20 s, and Cancel shows "Cancelling…" (tests
   `stalled_download_fails_instead_of_hanging`,
   `cancel_shows_cancelling_until_the_download_stops`).

## Known behaviour, not fixed here

- The Settings button in Android Auto: holding it starts the voice
  assistant, and the next tap is used to cancel the assistant rather than
  to open Settings. This is upstream input handling, unchanged here.
- Checking takes the Wi-Fi adapter from the access point, so wireless
  CarPlay and Android Auto pause while the updater is online. There is no
  automatic background check.
