# CT projection image

This directory is the downstream Yocto image for Spotify Car Thing. It pins
[`yocto-superbird`](https://github.com/JoeyEamigh/yocto-superbird) at
`bcba74441aa9e35b3a07cc8f087277e8281a5a46` and adds only the local
`meta-prelude` layer. Files under `sources/` are dependencies and must never
be edited as part of this project.

## Local build

Install `just`, Docker or Podman, `kas-container`, and `flashthing-cli`. On
macOS, initialize the ext4-backed Docker volume used for BitBake locks:

```sh
cd image
just macos-setup
just checkout
just parse
just build
```

The default target is `prelude-prod-local`. The Justfile mounts the repository
root read-write at `/monorepo`, while BitBake puts Cargo output in its own work
directory. Deploy artifacts are mirrored to:

```text
image/build/tmp/deploy/images/superbird/
```

There are three KAS profiles:

- `kas/prelude.yml` is the shared base: the pinned BSP, the `prelude` distro,
  and the sstate and hash-equivalence mirrors.
- `kas/prelude-prod.yml` builds `prelude-prod-image` from an immutable
  remote source revision.
- `kas/prelude-prod-local.yml` builds the same image from this checkout through
  `EXTERNALSRC`.

Other images (`prelude-dev-image`, `prelude-usb-host-test-image`,
`prelude-update`) are built with `bitbake` from `just shell`, which opens
the local production profile.

The development image loads `/etc/prelude/config-dev.toml`, which explicitly
opts into automatic phone selection. An iPhone uses configuration 6, the
private Lockdown carkit carrier, and a bounded first-pair flow that still
requires accepting the Trust dialog. An Android phone must pass a positive AOA
protocol probe before the daemon switches it into accessory mode; unrelated hub,
audio, Ethernet, and storage devices are never treated as phones.
The production image uses the same live profile. The daemon's packaged
default, `/etc/prelude/config.toml`, is fail-closed: it disables new pairing
and leaves projection disabled until an image installs the live override.

The live profile restores the pinned research pair-setup code in
`/var/lib/prelude/airplay/setup-code` as a root-owned mode `0600` file on
every boot, repairing stale or empty persistent state.
The bootloader uses the normal Nocturne mark. Weston and the daemon start with
the CarPlay splash, then the daemon switches to the selected platform's
CarPlay or Android Auto waiting mark while that phone's first decoded frame is
negotiated. The waiting mark is replaced atomically by the first decoded
projection frame and remains visible after disconnect. The selected platform is
persisted in `/var/lib/prelude/last-platform`, so the same waiting mark is
shown again on the next boot until another platform is selected.
The development receiver and input switches automatically select the single
connected USB DAC that exposes an ALSA playback PCM. Apple and non-Apple USB
Audio Class adapters use the same path; multiple physical DACs fail closed
instead of being selected by enumeration order.
All images include the daemon-owned Nocturne-style settings surface. A short
press of the physical M button opens the menu even during a projection session;
the menu overlays the session and captures local controls until it closes. The
menu controls automatic brightness, automatic night mode, and the shared
reported DPI, with a restore-original DPI action. DPI changes are committed
when the menu closes and rebuild the projection runtime before it resumes.
With automatic brightness on, `prelude-backlight.service` sets the backlight
from the ambient-light sensor early in boot, before preluded starts.
systemd-backlight is masked so it cannot restore a stale saved level.
The Phones page lists up to four wireless phones, most recent first. Connect
ends the current wireless session and pages the chosen phone, ignoring other
phones for a minute; Forget (pressed twice to confirm) removes the phone and
its Bluetooth bond.
After a long M press starts the voice assistant, the next short M press cancels
that action instead of opening settings.
All images support wired and wireless projection; see
[Wireless projection](#wireless-projection). Development selects
the built-in Car Thing microphone and discovered USB DAC by default. The daemon publishes a private runtime
snapshot under `/run/prelude`, and the CLI accepts it only with a
current-boot fresh heartbeat.

For a remote-source build, set an immutable fetch URI and revision and select
the remote production KAS profile:

```sh
PRELUDE_MONOREPO_SRC_URI='git://example.invalid/prelude.git;protocol=https;branch=main;destsuffix=${BP}' \
PRELUDE_MONOREPO_SRCREV='<40-character-commit>' \
just build prelude-prod
```

There is intentionally no default Git host or `AUTOREV`.

## Flash and USB-host bring-up

Enter Amlogic mask-ROM mode with the hub disconnected, then flash the
direct-OTG image:

```sh
just flash
```

`just flash` defaults to `prelude-prod-image`; pass another image name,
such as `prelude-dev-image`, after building it from `just shell`.

The pinned Yocto-Superbird BSP has a direct manual-host implementation: its
device tree exposes the OTG role switch and `superbird-usb-role to-host`
selects it at runtime. Upstream explicitly describes a powered OTG hub because
the port is also Car Thing's power input. Test that topology with a legacy USB
2.0 powered-OTG adapter/hub, not a laptop USB-C/PD dock. Start with a plain
flash drive and require real external enumeration in the timed Linux host trial
before connecting the iPhone or confirming host mode.

Direct charge-through OTG is the only supported topology. The Car Thing owns
the iPhone and USB DAC directly; no external computer, USB forwarding service,
or virtual host-controller payload is part of any image.

### Projection images

Both projection images install the direct-OTG topology: USB host mode is what
powers the hub, phone, DAC, and Wi-Fi adapter.

- `prelude-dev-image` automatically enters USB host mode on every normal
  boot for a qualified charge-through OTG adapter. The immutable OTG topology
  policy uses the same snapshot-first MAX20332 transaction and device-mode
  rollback as bounded trials; an interrupted transition is restored instead
  of retried during that boot. Hold the physical Back button while applying
  power to keep that boot in USB-device recovery mode; the button must remain
  held until userspace samples it. Missing or unreadable GPIO input also fails
  safely to device mode. The public `device-mode` command remains a temporary
  recovery escape until the next reboot. Its live daemon policy uses a
  15-second configuration reconnect, 5-second mux I/O, factual 86x52 mm
  display, 30 fps, USB DAC, built-in microphone, input, and media settings. It
  owns the iPhone or Android phone directly through libusb and automatically
  starts the matching protocol runtime. Build it with `bitbake
  prelude-dev-image` from `just shell` and flash it with
  `just flash prelude-dev-image`.
- `prelude-prod-image` is the production-footprint live counterpart: it
  keeps persistent direct OTG host mode and the same recovery payload, but
  omits the development tools while enabling the live automatic projection
  configuration and the CarPlay pairing flow. Its recovery gadget and ADB
  units stay out of the
  normal boot transaction. Holding Back/ESC during the startup sample keeps USB
  device mode and starts those units on demand. This is the default image:
  build it with `just build` and flash it with `just flash`.
- `prelude-update` wraps the `prelude-dev-image` boot FAT image and
  squashfs-lz4 root filesystem in the BSP SWUpdate A/B format. Apply it only to
  the inactive slot selected from `slot_active`; U-Boot gives the new slot
  three attempts before falling back to the previously active slot. Build it
  alongside the image with
  `bitbake prelude-dev-image prelude-update` from `just shell`.

First qualify a new adapter with `prelude-usb-host-test-image`; only after
external enumeration passes should the always-host OTG image be flashed. The
dedicated host-test image retains its gadget/recovery policy and never inherits
the automatic-host override.

The direct UGREEN 15598/Revodok topology is rejected. A completed 300-second
hardware trial proved the MAX20332 policy and DWC host transition, but the hub
and every device behind it produced zero external USB enumeration. See
[MAX20332 host trial](../docs/hardware/hil-max20332-host-trial-2026-08-16.md).

A source-powered USB-C laptop dock or PD injector is not a documented fix when
it requires its attached host to implement USB-PD data-role swap: the Car Thing
has no CC/PD controller, and U-Boot or kernel host mode cannot synthesize that
electrical protocol.

The dedicated `prelude-usb-host-test-image` performs the bounded Linux host
qualification trial. It records the USB topology, requires a real external
device beyond the controller root hub, and restores device mode before the
normal projection image is used.

Once an adapter has passed that gate, connect the TP-Link UE306 before changing
USB role. Its AX88179 driver supplies the development SSH path after CDC-NCM
disappears. The adapter's RJ45 port connects to the Mac or router. A USB cable
from another hub port to the Mac cannot replace it: in this mode both the Car
Thing and Mac are USB hosts, so neither side can enumerate the other as a USB
network device.
Public recovery commands are:

```sh
preludectl host-trial --timeout 300
preludectl host-trial-next-boot --timeout 300
preludectl host-confirm
preludectl device-mode
preludectl doctor --json
preludectl diagnostics export
```

### Visible USB-host qualification image

`prelude-usb-host-test-image` is the hands-off dock qualification variant.
It boots with the recovery gadget first, starts a 150-second timed host trial,
and shows live root-hub, external-device, and external-hub counts full-screen.
It observes the downstream link for 60 seconds, holds PASS or FAIL for 20
seconds, then explicitly restores USB-device mode. The independent role timer
remains armed as a crash fallback. It never confirms persistent host mode.

The image disables automatic projection so it cannot claim the test hardware.
Each boot writes redacted evidence under
`/var/lib/prelude/diagnostics/usb-host-test/runs/<boot-id>` and atomically
updates `latest-run`; reconnecting through the Mac therefore cannot overwrite
the preceding dock run. USB serial strings are not recorded. Build the
qualification image from the local OTG KAS shell:

```sh
just shell
# Run inside the opened shell:
bitbake prelude-usb-host-test-image
exit
# On macOS, copy the new deploy artifact out of the build volume:
just pull-deploy
```

Then flash it with `just flash prelude-usb-host-test-image`. Test the powered
adapter/dock with only a plain flash drive first. `PASS` requires a real
external USB device beyond controller root hubs. After the
screen reports that device mode was restored, remove the dock and reconnect
the Mac to recover ADB and collect the diagnostic directory.

`host-confirm` must be run inside the live SSH session carried by the UE306.
The controller verifies that `SSH_CONNECTION` terminates on a carrier-up
AX88179/RTL8152 interface before it writes the persistent-host marker.

At boot, `prelude-usb-host.service` calls the internal `host-boot` command.
That service is condition-gated until the persistent host flag has been
confirmed. `host-trial-next-boot` only writes a bounded, one-shot request while
the port is still safely in device mode. On the next boot,
`prelude-host-trial-next-boot.service` consumes that marker exactly once,
after the persistent-host check and recovery gadget startup but before the
daemon, then enters the ordinary timed trial with automatic device-mode
fallback. Development images boot with the BSP CDC-NCM/ADB gadget enabled for
direct-USB recovery. A host trial stops the gadget before switching roles, and
the timeout or `device-mode` restores it. The production image includes the
gadget disabled for Back-held, on-demand recovery.
Mask-ROM flashing remains the fallback if neither host Ethernet nor device-mode
networking is available.

## Wireless projection

Every image keeps the onboard BCM20703A2 Bluetooth controller and the USB
Wi-Fi stack. Bluetooth carries the bootstrap: iAP2 for CarPlay and the
Hands-Free plus Wi-Fi projection RFCOMM channels for Android Auto. The phone
then joins a 5 GHz access point that `preluded` runs on the first USB Wi-Fi
adapter it finds, using its own `hostapd` and DHCP-only `dnsmasq`. The system
`dnsmasq` and `hostapd` units stay disabled. With Phone audio on, `preluded`
drops the Hands-Free link once projection is running, so the phone keeps calls
on its own speaker and microphone.

Android Auto calls always travel over Bluetooth Hands-Free, wired or wireless.
With Phone audio off, `preluded` advertises the controller address in Android
Auto service discovery so the phone pairs for calls on its own. The BlueZ agent
uses numeric comparison, and `preluded` forwards the passkey over Android Auto
so the phone can confirm the pairing itself. It then plays
the SCO call audio through the USB DAC and sends the built-in microphone back.
`preluded` cancels the speaker echo and suppresses noise on that microphone
itself, and sends `AT+NREC=0` after the Hands-Free link comes up so the phone
turns its own echo and noise reduction off instead of stacking a second pass.
Calls use HD voice (mSBC, 16 kHz, encoded in `preluded`) when the phone
negotiates it, otherwise CVSD at 8 kHz. Android only picks mSBC when the
Hands-Free SDP record advertises wide band speech, so `preluded` registers it
with `Features` (`0x3c`, or `0x1c` once calls fall back to narrowband). If no mSBC frame decodes within 2 s,
`preluded` closes that link, offers only CVSD (`AT+BAC=1`), asks an active call
to reopen its audio (`AT+BCC`), and writes `narrowband-calls` to its wireless
state directory so later calls stay on CVSD; delete that file to try HD voice
again. Once the controller is up, `preluded` runs
`hcitool cmd 0x3f 0x1c` to route SCO over the UART rather than the BCM PCM pins.
Without a Wi-Fi adapter, Bluetooth stays on only for Hands-Free and stays
hidden: it registers no iAP2 or Android Auto wireless records and is pairable
but not discoverable. If Phone audio is also on, Bluetooth powers off instead.
CarPlay calls ride the CarPlay session's telephony stream instead.

- The Bluetooth name is the configured `head_unit_name` (`Prelude`). The SSID is
  `Prelude-XXXX`, derived from the adapter address, and the access point is
  `10.47.0.1/24` on channel 36 at 80 MHz.
- Every mainline USB driver with AP-mode support is installed (mt76, rtw88,
  rtw89, rtl8xxxu, ath9k_htc, carl9170, rt2800usb) along with its firmware, so
  any compatible adapter works without image changes. The adapter may be
  hotplugged; the daemon waits for it.
- `PRELUDE_WIRELESS_COUNTRY` (default `US`) sets the regulatory domain, and
  `PRELUDE_WIRELESS_PASSPHRASE` pins the WPA2 passphrase so a laptop can join
  and SSH to `10.47.0.1` while the Car Thing is in USB host mode. Both are read
  from the environment by `just build`:

  ```sh
  PRELUDE_WIRELESS_COUNTRY=CA PRELUDE_WIRELESS_PASSPHRASE=... just build
  ```

- The wireless settings are appended to `/etc/prelude/config-dev.toml`, the
  live profile both projection images load.
- The journal is persistent. Wireless failures happen with the hub attached,
  so hold Back while powering on to boot into USB recovery, then read the
  failed boot with `journalctl -b -1 -u preluded`.

## Verification

`just lint` checks the pin, local-source isolation, packaged binaries, service
CLI contract, required kernel module packages, and radio image policy.
`just parse` runs BitBake's parser and `bitbake-layers show-appends` in the same
KAS environment used for a build. A hardware release must additionally verify
the powered hub, iPhone and USB DAC enumeration, voltage/backfeed limits,
repeated cold boots/hotplug, USB audio soak, and mask-ROM recovery.

## License

The downstream integration in this directory is licensed under
GPL-3.0-only. Upstream layers and packages retain their own licenses.

The media stack retains its component licenses: the Opus, RTP, AAC parser,
and other standard GStreamer plugins are LGPL-2.1-or-later; libopus is
BSD-3-Clause; the FAAD plugin is GPL-2.0-or-later; and FAAD2 is
GPL-2.0-only. `commercial_faad2` is accepted narrowly because OpenEmbedded
uses that policy flag for FAAD2; accepting it does not change the software's
license. Image builds deploy the package and image license manifests under
`tmp/deploy/licenses/`.

The direct-OTG topology is hardware-decode-only. The downstream kernel
propagates the firmware error flag to V4L2, repairs stale input accounting,
recovers unmatched low-latency H.264 output from the oldest queued timestamp,
and avoids a transient mem2mem `EPOLLERR` before the stateful decoder's first
source-change event. The image does not install gst-libav or accept FFmpeg's
OpenEmbedded policy flag.

A flashable ZIP is not by itself a complete redistributable release. Publish
the corresponding sources, patches, layer metadata, and workspace revision
used for every GPL-covered binary alongside any distributed image.
