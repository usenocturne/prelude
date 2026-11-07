<h1 align="center">
  <img src="https://usenocturne.com/images/logo.png" alt="Nocturne" width="200">
  <br>
  Prelude
  <br>
</h1>

<p align="center">Wireless and Wired Apple CarPlay and Android Auto for the <a href="https://carthing.spotify.com" target="_blank">Spotify Car Thing</a>.</p>

<p align="center">
  <a href="#flashing">Flashing</a> •
  <a href="#donate">Donate</a> •
  <a href="#building">Building</a> •
  <a href="#layout">Layout</a> •
  <a href="#license">License</a>
</p>

<div align="center">
  <a href="https://usenocturne.com"><img alt="Website" src="https://img.shields.io/badge/website-gray?style=flat-square&logo=react&logoColor=FFFFFF"></a>
  <a href="https://discord.gg/GTP9AawHPt"><img alt="Discord" src="https://img.shields.io/discord/1304909652387172493?style=flat-square&logo=discord&logoColor=FFFFFF&label=discord"></a>
</div>

<br>

<p align="center"><img width=600 src="https://usenocturne.com/images/prelude.png" alt="Prelude screenshot"></p>

## Setup

> [!WARNING]
> Bricking the Car Thing is nearly impossible, but the risk is always there when flashing custom firmware.

### Requirements

- Terbium driver is required on Windows: `irm https://driver.terbium.app/get | iex` (Powershell)
- A USB OTG adapter and USB hub is required to use this. Some USB hubs may support USB OTG on their own, but your milage may vary. [This Anker hub](https://www.amazon.com/dp/B0BQLLB61B) works if you do not want to buy a USB OTG adapter. Otherwise, we recommend [this USB OTG adapter](https://www.amazon.com/dp/B08KPD5S82) and a standard USB hub.
- A USB Wi-Fi adapter is required if you would like to use Wireless CarPlay/Android Auto. Any adapter with Linux support and Access Point mode should work. We recommend [this adapter](https://www.amazon.com/dp/B07PB1X4CN).
- A USB DAC if your USB hub does not have audio out. Without one, turn on **Phone audio** in settings to keep sound on your phone (speaker, Bluetooth, etc.). We recommend the [Apple USB-C to 3.5 mm Headphone Jack Adapter](https://www.amazon.com/Apple-USB-C-Headphone-Jack-Adapter/dp/B0D7FF5Y3X) paired with a USB C to A adapter if needed.

### Flashing

The recommended way to flash is to use [Terbium](https://terbium.app) with an installer zip from [Releases](https://github.com/usenocturne/prelude/releases/latest).

Flashing will likely take about 5-10 minutes, depending on your USB ports and some other factors. Please try multiple ports if one isn't working (Rear IO USB 3/2, BIOS flash port if on AMD, etc).

### Connecting to Prelude

Prelude supports both wired and wireless connections.

<details>
<summary><img src="https://camo.githubusercontent.com/b9c79d36777ba11fe5423f498b522f7b786898772a1ddbb44074fb6bc59adf06/68747470733a2f2f7573656e6f637475726e652e636f6d2f696d616765732f6c6f676f2e706e67" height="14" style="vertical-align: middle;"> Apple CarPlay (Wired)</summary>

1. Plug your USB OTG adapter into the Car Thing, and plug power into the USB-C port, and the USB hub into the USB-A port. If required, plug in your USB DAC into the hub as well.
2. Plug in your phone to the USB hub.
3. Follow the hints on screen. It is normal for the Car Thing to request trust from your phone before starting CarPlay for the first time.

</details>

<details>
<summary><img src="https://camo.githubusercontent.com/b9c79d36777ba11fe5423f498b522f7b786898772a1ddbb44074fb6bc59adf06/68747470733a2f2f7573656e6f637475726e652e636f6d2f696d616765732f6c6f676f2e706e67" height="14" style="vertical-align: middle;"> Apple CarPlay (Wireless)</summary>

1. Plug your USB OTG adapter into the Car Thing, and plug power into the USB-C port, and the USB hub into the USB-A port. Plug your USB Wi-Fi adapter (and USB DAC, if required) into the hub.
2. On your iPhone, make sure Bluetooth and Wi-Fi are on, then open **Settings → General → CarPlay → Available Cars** and pick **Prelude**.
3. Follow the prompts on your iPhone to finish setting up. 

</details>

<details>
<summary><img src="https://usenocturne.com/favicon.ico" height="14" style="vertical-align: middle;"> Android Auto (Wired)</summary>

1. Plug your USB OTG adapter into the Car Thing, and plug power into the USB-C port, and the USB hub into the USB-A port. If required, plug in your USB DAC into the hub as well.
2. Plug in your phone to the USB hub.
3. Follow the hints on screen. It is normal for your phone to show a "Connecting to Android Auto" notification each time.   

</details>

<details>
<summary><img src="https://usenocturne.com/favicon.ico" height="14" style="vertical-align: middle;"> Android Auto (Wireless)</summary>

1. Plug your USB OTG adapter into the Car Thing, and plug power into the USB-C port, and the USB hub into the USB-A port. Plug your USB Wi-Fi adapter (and USB DAC, if required) into the hub.
2. On your phone, open **Settings → Connected devices → Pair new device** and pick **Prelude**.
3. Follow the prompts on your phone to finish setting up.

</details>

### Controls
- Navigate by using the touch screen or the knob.
- Press the knob to confirm, or back to exit.
- By default, the four preset buttons skip back a track, play/pause, skip forward a track, and toggle mute. To change them, see [Preset buttons](#preset-buttons).
- Press right-most top hardware button to open settings.
- Hold the right-most top hardware button to trigger Siri/Google Assistant.

### Preset buttons

Open **Settings → Preset buttons** to choose what each of the four top buttons does, numbered left to right. The options are previous track, play/pause, next track, mute, home, back, voice assistant (Siri or Google Assistant), answer call, end call, or nothing. Every option works on both CarPlay and Android Auto. The new mapping applies as soon as settings close.

### Equalizer

Open **Settings → Equalizer** to adjust the ten bands (31.25 Hz to 16 kHz) and preamp.

Drag a band vertically or the preamp horizontally. With the knob, rotate to select a control, press to edit, and rotate to adjust in 0.5 dB steps. Press again to finish editing. Adjustments take effect live and save when you release a slider or finish an edit. Negative preamp cannot guarantee clipping-free output for every signal and curve.

The equalizer affects all USB DAC output, including music, navigation, calls and assistants, for both CarPlay and Android Auto. **Phone audio bypasses it**, and the microphone is unaffected.

### Uninstall

Use a tool of your choice (`flashthing-cli` with stock firmware, or Terbium for the old Buildroot path) to flash stock or a different firmware.

## Donate

Prelude and [Nocturne](https://github.com/usenocturne/nocturne) are massive endeavors, and the team has spent every day over the last year making it a reality out of our passion for creating something that people like you love to use.

All donations are split between the three members of the Nocturne team and go towards the development of future features. We are so grateful for your support!

[Donation Page](https://usenocturne.com/support)

## Building

Prelude is a monorepo. Prelude OS, `preluded` (the on-device daemon), `preludectl`,
and the supporting Rust crates all live in the same tree and build through the
same `Justfile` at the repo root.

### Required host tools

| Tool | Why |
|---|---|
| [`just`](https://github.com/casey/just) | drives the recipes in the top-level and per-component `Justfile`s |
| [`cargo`](https://rustup.rs/) + [`cross`](https://github.com/cross-rs/cross) | builds the daemon, cross-compiled to aarch64 for the Car Thing |
| `docker` or `podman` | runs the kas container for the image build |
| [`kas`](https://kas.readthedocs.io/) | invoked through `kas-container` |
| [`flashthing-cli`](https://crates.io/crates/flashthing-cli) | host-side burn-mode flasher |
| `python3` | runs the image validation and test scripts |
| `rclone` (optional) | publishes new Yocto sstate objects with `just image-push-sstate` |

### The full image

```bash
just image-build                              # default: prelude-prod-local
PRELUDE_WIRELESS_COUNTRY=CA just image-build   # set the Wi-Fi regulatory domain (default: US)
```

| KAS profile | What it is |
|---|---|
| `prelude` | Shared base: the pinned `yocto-superbird` layer, distro, and sstate/hash-equivalence mirrors. Every other profile includes it. |
| `prelude-prod` | The production image (`prelude-prod-image`) built from an immutable remote source revision. |
| `prelude-prod-local` | The same image built from this checkout. This is the default. |

Every image supports both wired and wireless CarPlay/Android Auto; there is no
separate wireless profile. Two optional environment variables configure the
wireless access point at build time:

- `PRELUDE_WIRELESS_COUNTRY`: ISO 3166-1 alpha-2 regulatory domain (default `US`).
- `PRELUDE_WIRELESS_PASSPHRASE`: fixed 8-63 character WPA2 passphrase, so a laptop
  can join the access point and SSH to `10.47.0.1` while the Car Thing is in USB
  host mode. When unset, the passphrase is the Car Thing's serial number with only its
  letters and digits kept (a random one is generated only if the serial can't be read).

The first cold build downloads upstream layers, crates, and tarballs (multiple
gigabytes). Subsequent builds reuse the BitBake state and compiler cache. The
Prelude KAS configuration uses the project sstate mirror at
`https://sstate.bridgething.com/sstate/` and hash-equivalence service at
`wss://hashserv.bridgething.com/ws`, with the public Yocto mirror as a fallback.

For local profiles, the daemon and control utility sources are mounted directly
from this checkout through Yocto `EXTERNALSRC`; there is no separate source
publish or revision-bump round trip while iterating. Remote-source profiles
require immutable `PRELUDE_MONOREPO_SRC_URI` and `PRELUDE_MONOREPO_SRCREV` values;
see [`image/README.md`](image/README.md) for the remote-source and USB-host
workflows.

Outputs land in `image/build/tmp/deploy/images/superbird/`, or under the
directory selected by `YOCTO_BUILD_DIR`. The default build produces
`prelude-prod-image-superbird-flashthing.zip`.

The development image (`prelude-dev-image`, with diagnostic tools and boot-time
USB recovery) and the USB host qualification image are built from a KAS shell:

```bash
just image-shell
bitbake prelude-dev-image    # run inside the shell, then exit
just image-pull-deploy       # macOS only: copy the artifacts out of the build volume
```

### Daemon

```bash
just daemon-host              # cargo build for the development host
just daemon-build             # cross build for aarch64 Car Thing hardware
just build                    # native workspace build
just build-release            # release workspace build
just test                     # workspace tests
just lint                     # cargo fmt --check + cargo clippy
```

`just check` runs the Rust checks and the image-layer test suite. The image build
compiles both `preluded` and `preludectl` through the `prelude-runtime` recipe.

### Image build and flashing

The image's [`image/Justfile`](image/Justfile) is the canonical command surface
for KAS builds and device flashing:

```bash
just image-macos-setup                         # macOS only
just image-checkout prelude-prod-local
just image-parse prelude-prod-local
just image-shell prelude-prod-local
just image-flash prelude-prod-image
just image-flash-env prelude-prod-image
```

For a manual macOS build from an image shell, `just image-pull-deploy` mirrors
deploy artifacts out of the Docker build volume. `just image-clean-build` drops
BitBake output while retaining checked-out layers and the compiler cache;
`just image-vscode-setup` creates the standard Yocto links for the VS Code
BitBake extension. Use `just image-push-sstate` only after a successful build
when publishing new cache objects is intended.

## Layout

| Path | What lives there |
| --- | --- |
| [`image/`](image) | The Yocto image, KAS profiles, downstream layer, image scripts, tests, and the canonical image `Justfile`. |
| [`crates/daemon/`](crates/daemon) | The `preluded` projection daemon, media runtime, and native settings UI. |
| [`crates/ctl/`](crates/ctl) | The `preludectl` device-control and diagnostics CLI. |
| [`crates/`](crates) | CarPlay, Android Auto, iAP2, MFi, media, USB, and Superbird support libraries. |
| [`image/README.md`](image/README.md) | Detailed image, direct-OTG, USB-host qualification, recovery, and verification notes. |

## Credits

This software was made possible only through the following individuals and open source projects:

- [Neel Patel](https://github.com/68p)
- [Dominic Frye](https://github.com/itsnebulalol)

We'd like to give a huge thanks to [Joey Eamigh](https://github.com/JoeyEamigh) for [bridgething](https://github.com/JoeyEamigh/bridgething) (robust iAP2, and more), [yocto-superbird](https://github.com/JoeyEamigh/yocto-superbird), and for a ton of help during the Yocto migration. We'd also like to thank [lmore377](https://github.com/lmore377) for modernizing the Car Thing's tooling and contributions to bridgething/yocto-superbird.

- [The Yocto Project](https://www.yoctoproject.org/) and [OpenEmbedded](https://www.openembedded.org/)
- [LIVI](https://github.com/f-io/LIVI)
- [aasdk / OpenAuto](https://github.com/f1xpl/aasdk)
- [pymobiledevice3](https://github.com/doronz88/pymobiledevice3)

## License

Prelude is licensed under the **GPL-3.0** license.

We kindly ask that any modifications or distributions made outside of direct forks from this repository include attribution to the original project in the README, as we have worked hard on this. :)

---

> © 2026 Vanta Labs.

> "Spotify" and "Car Thing" are trademarks of Spotify AB. This software is not affiliated with or endorsed by Spotify AB.

> [usenocturne.com](https://usenocturne.com) &nbsp;&middot;&nbsp;
> [GitHub](https://github.com/usenocturne) &nbsp;&middot;&nbsp;
> [X](https://x.com/usenocturne) &nbsp;&middot;&nbsp;
> [Discord](https://discord.gg/GTP9AawHPt)
