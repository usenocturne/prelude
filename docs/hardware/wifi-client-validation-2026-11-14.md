# Wi-Fi client validation on hardware, 2026-11-14

Branch `feat/ota-wifi-client`, Plan 2 Task 7. One contributor Car Thing
(superbird), kernel 7.0.2-superbird, TP-Link AC600 Nano (USB 2357:011e, Realtek
RTL8821AU, `rtw88_8821au`) on a powered USB hub.

## Setup

| Artifact | Contents |
|---|---|
| 1.0.8-test | `prelude-update-prod`, test key, this branch at the Settings > Internet commit |
| 1.0.11-test | `prelude-update-prod`, test key, this branch with the two Task 7 fixes, plus two Android Auto USB fixes from a separate branch and a temporary message trace (neither committed here) |

Bundles were installed over the access point with SSH (test images allow root
without a password), as in the 2026-11-14 retest in
`ota-validation-2026-11-13.md`. Every trial boot was marked good. Networks
used: a home WPA2 router and an Android phone hotspot (WPA2). Device logs are
kept with the contributor.

## Results

| Step | Check | Result |
|---|---|---|
| 1 | 1.0.8-test installs over Wi-Fi; `wpa_supplicant` and `/usr/libexec/prelude-udhcpc-script` present | Pass |
| 2 | Phone hotspot appears, joins with its password, `Online via <hotspot> · <address>` | Pass: lease in 10.176.189.0/24, reachability check passed |
| 3a | Wrong password | Pass: `WRONG_KEY` from `wpa_supplicant`, "Wrong password" on screen, access point back 1.3 s later |
| 3b | Hotspot turned off, then chosen from the list | Pass: authentication timed out, "Network not found" after about 25 s |
| 3c | Adapter pulled while online | Pass: "Wi-Fi adapter removed", access point back 5 s after replug |
| 4 | Leaving Settings > Internet restores the access point | Pass every time: same SSID, channel 36, 20 MHz as `settings.toml`; no leftover default route, per-link DNS or run directory |
| 5 | Home Wi-Fi joins; with the saved network, Test connection reconnects without the keyboard | Pass, four times on 1.0.11-test |

Privacy checks after joining: `wifi-networks.toml` is mode 0600 and holds only
the hex SSID and the derived PSK; the journal has no match for the passphrase,
`psk` or `SET_NETWORK`.

## Findings fixed during the run

- The Settings row read "Internet  Off" after leaving the page, which looked
  like broken Wi-Fi. It now names the saved network ("<ssid> · not
  connected") or says "Not connected" (test
  `internet_row_names_the_network_instead_of_off`).
- While online, the Bluetooth side kept paging the phone for wireless Android
  Auto every 12 s and failing, because the access point was parked. Wireless
  projection, CarPlay included, is now held while client mode is requested
  (test `wireless_projection_is_held_while_online_or_projecting_over_usb`).

## Known behaviour, not fixed here

- A wireless Android Auto session that is running when the user opens
  Settings > Internet ends by ping timeout about 12 s after the access point
  parks, rather than at once. The page footer already says wireless projection
  pauses while online.
- Choosing a network that has gone away takes about 25 s to report "Network
  not found" (the join timeout).
- Leaving Settings > Internet always hands the adapter back to the access
  point, by design. The updater (Plan 3) joins the saved network on its own
  when checking for updates.
