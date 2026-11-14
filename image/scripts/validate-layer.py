#!/usr/bin/env python3
"""Fast, dependency-free checks for errors that otherwise surface late in BitBake."""

from pathlib import Path
import hashlib
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
WORKSPACE = ROOT.parent
PIN = "bcba74441aa9e35b3a07cc8f087277e8281a5a46"


def fail(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def text(path: str) -> str:
    target = ROOT / path
    if not target.is_file():
        fail(f"missing required file: {path}")
    return target.read_text(encoding="utf-8")


kas = text("kas/prelude.yml")
distro = text("meta-prelude/conf/distro/prelude.conf")
recipe = text("meta-prelude/recipes-core/prelude/prelude-runtime_0.1.0.bb")
unit = text("meta-prelude/recipes-core/prelude/files/prelude-usb-host.service")
trial_unit = text(
    "meta-prelude/recipes-core/prelude/files/"
    "prelude-host-trial-next-boot.service"
)
daemon_unit = text("meta-prelude/recipes-core/prelude/files/preluded.service")
cargo_manifest = (WORKSPACE / "Cargo.toml").read_text(encoding="utf-8")
daemon_dev = text("meta-prelude/recipes-core/prelude/files/preluded-dev.conf")
daemon_env = text("meta-prelude/recipes-core/prelude/files/preluded.env")
tmpfiles = text(
    "meta-prelude/recipes-core/prelude/files/prelude.tmpfiles.conf"
)
dev_tmpfiles = text(
    "meta-prelude/recipes-core/prelude/files/prelude-dev.tmpfiles.conf"
)
config = text("meta-prelude/recipes-core/prelude/files/config.toml")
config_dev = text("meta-prelude/recipes-core/prelude/files/config-dev.toml")
health_unit = text(
    "meta-prelude/recipes-core/prelude/files/prelude-host-health.service"
)
recovery_unit = text(
    "meta-prelude/recipes-core/prelude/files/prelude-device-recovery.service"
)
recovery_path = text(
    "meta-prelude/recipes-core/prelude/files/prelude-device-recovery.path"
)
packagegroup = text(
    "meta-prelude/recipes-core/packagegroups/packagegroup-prelude-core.bb"
)
image_base = text("meta-prelude/recipes-core/images/prelude-image-base.inc")
kernel_config = text(
    "meta-prelude/recipes-kernel/linux/linux-superbird/prelude-usb.cfg"
)
kernel_policy = text("meta-prelude/recipes-kernel/linux/linux-superbird_%.bbappend")
dev_update_recipe = text(
    "meta-prelude/recipes-core/prelude-update/prelude-update-dev.bb"
)
prod_update_recipe = text(
    "meta-prelude/recipes-core/prelude-update/prelude-update-prod.bb"
)
update_signing = text(
    "meta-prelude/recipes-core/prelude-update/prelude-update-signing.inc"
)
update_sw_description = text(
    "meta-prelude/recipes-core/prelude-update/files/sw-description"
)
dev_packagegroup = text(
    "meta-prelude/recipes-core/packagegroups/packagegroup-prelude-dev.bb"
)
bsp_packagegroup = text(
    "meta-prelude/recipes-core/packagegroup-superbird-runtime/packagegroup-superbird-runtime.bbappend"
)
gadget_policy = text(
    "meta-prelude/recipes-core/superbird-usb-gadget/superbird-usb-gadget_%.bbappend"
)
adbd_policy = text(
    "meta-prelude/recipes-devtools/android-tools/android-tools_%.bbappend"
)
role_helper = text(
    "meta-prelude/recipes-bsp/superbird-usb-role/files/superbird-usb-role"
)
role_policy = text(
    "meta-prelude/recipes-bsp/superbird-usb-role/superbird-usb-role_%.bbappend"
)
uboot_append = ROOT / (
    "meta-prelude/recipes-bsp/superbird-uboot/superbird-uboot_%.bbappend"
)
uboot_probe = ROOT / (
    "meta-prelude/recipes-bsp/superbird-uboot/files/"
    "0001-carthing-add-reversible-usb-host-probe.patch"
)
gst_base_policy = text(
    "meta-prelude/recipes-multimedia/gstreamer/gstreamer1.0-plugins-base_%.bbappend"
)
gst_bad_policy = text(
    "meta-prelude/recipes-multimedia/gstreamer/gstreamer1.0-plugins-bad_%.bbappend"
)
obsolete_bluealsa_policy = ROOT / (
    "meta-prelude/recipes-multimedia/bluealsa/bluealsa_%.bbappend"
)
superbird_init_policy = text(
    "meta-prelude/recipes-core/superbird-init/superbird-init.bbappend"
)
superbird_init_script = text(
    "meta-prelude/recipes-core/superbird-init/files/superbird-init.sh"
)
superbird_init_unit = text(
    "meta-prelude/recipes-core/superbird-init/files/superbird-init.service"
)
superbird_metadata_template = text(
    "meta-prelude/recipes-core/superbird-init/files/superbird-meta.json.in"
)
host_test_recipe = text(
    "meta-prelude/recipes-core/prelude-host-test/prelude-host-test_0.1.0.bb"
)
host_test_unit = text(
    "meta-prelude/recipes-core/prelude-host-test/files/"
    "prelude-host-test.service"
)
host_test_display_unit = text(
    "meta-prelude/recipes-core/prelude-host-test/files/"
    "prelude-host-test-display.service"
)
host_test_runner = text(
    "meta-prelude/recipes-core/prelude-host-test/files/prelude-host-test"
)
host_test_image = text(
    "meta-prelude/recipes-core/images/prelude-usb-host-test-image.bb"
)
prod_kas = text("kas/prelude-prod.yml")
prod_local_kas = text("kas/prelude-prod-local.yml")
topology_recipe = text(
    "meta-prelude/recipes-core/prelude-topology/prelude-topology_0.1.0.bb"
)
session_recovery_script = text(
    "meta-prelude/recipes-core/prelude-topology/files/prelude-session-recover"
)
otg_recovery_service = text(
    "meta-prelude/recipes-core/prelude-topology/files/"
    "prelude-otg-recovery.service"
)
otg_recovery_path = text(
    "meta-prelude/recipes-core/prelude-topology/files/"
    "prelude-otg-recovery.path"
)
otg_always_host = text(
    "meta-prelude/recipes-core/prelude-topology/files/otg-always-host.conf"
)
otg_host_health = text(
    "meta-prelude/recipes-core/prelude-topology/files/otg-host-health.conf"
)
otg_image = text("meta-prelude/recipes-core/images/prelude-dev-image.bb")
prod_otg_image = text("meta-prelude/recipes-core/images/prelude-prod-image.bb")
logo_overlays = {
    "CarPlay": ROOT / "meta-prelude/recipes-core/prelude/files/carplay-logo.nv12",
    "Android Auto": ROOT
    / "meta-prelude/recipes-core/prelude/files/android-auto-logo.nv12",
}

if f"commit: {PIN}" not in kas:
    fail("yocto-superbird is not pinned to the reviewed commit")
for profile in (prod_kas, prod_local_kas):
    if "prelude-prod-image" not in profile:
        fail("production KAS profile does not select prelude-prod-image")
if (
    'EXTERNALSRC:pn-prelude-runtime = "${PRELUDE_LOCAL_MONOREPO}"'
    not in prod_local_kas
):
    fail("local production profile does not retain EXTERNALSRC isolation")
for obsolete_kas in (
    "prelude-otg.yml",
    "prelude-otg-local.yml",
    "prelude-prod-otg.yml",
    "prelude-prod-otg-local.yml",
    "prelude-wireless-otg.yml",
    "prelude-wireless-otg-local.yml",
):
    if (ROOT / "kas" / obsolete_kas).exists():
        fail(f"obsolete KAS profile remains: kas/{obsolete_kas}")
for binary in ("preluded", "preludectl"):
    if not re.search(rf"install -m 0755 .*\b{binary}\b", recipe):
        fail(f"Rust recipe does not install {binary}")
if "ExecStart=/usr/bin/preludectl host-boot" not in unit:
    fail("USB host unit does not use the locked host-boot interface")
if "ConditionPathExists=/var/lib/prelude/persistent-host-enabled" not in unit:
    fail("USB host unit is not gated on confirmed persistent host mode")
if "ExecStart=/usr/bin/preludectl consume-host-trial-next-boot" not in trial_unit:
    fail("next-boot host-trial unit does not use the locked boot consumer")
if "ConditionPathExists=/var/lib/prelude/host-trial-next-boot" not in trial_unit:
    fail("next-boot host-trial unit is not gated on its one-shot marker")
if (
    "After=superbird-usb-gadget.service android-tools-adbd.service prelude-usb-host.service"
    not in trial_unit
):
    fail("next-boot host-trial consumer can race gadget or persistent-host startup")
if "Before=preluded.service" not in trial_unit:
    fail("daemon can start before a scheduled boot trial is consumed")
if "OnFailure=prelude-device-recovery.service" not in trial_unit:
    fail("next-boot host-trial failures do not schedule device-mode recovery")
if "TimeoutStartSec=90s" not in trial_unit:
    fail("next-boot host-trial deadline can interrupt bounded device-mode recovery")
if "WantedBy=multi-user.target" not in trial_unit:
    fail("next-boot host-trial consumer is not enabled in the boot transaction")
if "prelude-host-trial-next-boot.service" not in daemon_unit:
    fail("daemon is not ordered after the next-boot host-trial consumer")
if "StartLimitIntervalSec=0" not in daemon_unit:
    fail("daemon restart recovery can be exhausted by systemd start limiting")
if not re.search(r'^panic\s*=\s*"unwind"$', cargo_manifest, re.MULTILINE):
    fail("release panics bypass bounded worker cleanup and USB restoration")
for recipe_reference in (
    "file://prelude-host-trial-next-boot.service",
    "prelude-host-trial-next-boot.service",
    "${UNPACKDIR}/prelude-host-trial-next-boot.service",
):
    if recipe_reference not in recipe:
        fail("Rust runtime recipe does not package the next-boot host-trial unit")
if "ExecStart=/usr/bin/preludectl recover-if-required" not in recovery_unit:
    fail("interrupted-transition recovery does not recheck state under the role lock")
if "StartLimitIntervalSec=0" not in recovery_unit:
    fail("device recovery retries can be exhausted before a timed trial expires")
if "Restart=on-failure" not in recovery_unit or "RestartSec=60s" not in recovery_unit:
    fail("device recovery does not retain bounded, low-churn fail-safe retries")
if "PathExists=/var/lib/prelude/device-recovery-required" not in recovery_path:
    fail("no path watcher restores recovery after an interrupted transition")
if (
    "ExecStart=/usr/bin/preluded --config /etc/prelude/config.toml"
    not in daemon_unit
):
    fail("daemon unit does not load the packaged production-safe configuration")
stop_deadline_match = re.search(r"^TimeoutStopSec=(\d+)s$", daemon_unit, re.MULTILINE)
reconnect_timeout_match = re.search(
    r"^configuration_reconnect_timeout_ms\s*=\s*(\d+)$",
    config_dev,
    re.MULTILINE,
)
if stop_deadline_match is None or reconnect_timeout_match is None:
    fail("daemon stop deadline or live reconnect timeout is not declared in seconds")
stop_deadline_ms = int(stop_deadline_match.group(1)) * 1_000
reconnect_timeout_ms = int(reconnect_timeout_match.group(1))
# Mirrors configured_worker_stop_bound for the live image profile: the
# 30-second iAP2 stage competes with reveal plus two reconnect windows, then
# one final reconnect window is reserved for restoring the USB configuration.
configured_stop_bound_ms = max(30_000, 3_000 + 2 * reconnect_timeout_ms, 5_000, 2_000)
configured_stop_bound_ms += reconnect_timeout_ms
if stop_deadline_ms < configured_stop_bound_ms:
    fail(
        "daemon stop deadline is shorter than the live config-derived worker "
        f"stop bound ({stop_deadline_ms}ms < {configured_stop_bound_ms}ms)"
    )
if "After=dbus.service" not in daemon_unit:
    fail("daemon is not ordered after the system bus")
for forbidden_service in (
    "superbird-bluetooth.service",
    "bluetooth.service",
    "bluealsa.service",
):
    if forbidden_service in daemon_unit:
        fail(f"daemon unit retains obsolete radio dependency {forbidden_service}")
if (
    "ExecStart=/usr/bin/preluded --config /etc/prelude/config-dev.toml"
    not in daemon_dev
):
    fail(
        "development daemon override does not load the transport bring-up configuration"
    )
for opt_in in (
    "configuration = 6",
    "allow_private_apple_configuration = true",
    "allow_private_lockdown_service = true",
    "allow_first_time_pairing = true",
    'mode = "automatic"',
    "enable_live_session = true",
):
    if opt_in not in config_dev:
        fail(f"development transport configuration is missing {opt_in}")
for airplay_opt_in in (
    "enable_live_receiver = true",
    "enable_live_input = true",
):
    if airplay_opt_in not in config_dev:
        fail(f"development AirPlay gate is missing {airplay_opt_in}")
for airplay_opt_out in ("enable_live_receiver = false", "enable_live_input = false"):
    if airplay_opt_out not in config:
        fail(f"production AirPlay gate is missing {airplay_opt_out}")
if "[android_auto]" not in config_dev or "enable_live_session = true" not in config_dev:
    fail("development Android Auto gate is missing")
if "[android_auto]" not in config or "enable_live_session = false" not in config:
    fail("production Android Auto gate is missing")
if 'setup_code_file = "/var/lib/prelude/airplay/setup-code"' not in config_dev:
    fail(
        "development AirPlay configuration does not reference its provisioned setup code"
    )
if "setup_code_file" in config:
    fail("production configuration installs an AirPlay setup-code path")
if dev_tmpfiles.strip() != (
    "f+ /var/lib/prelude/airplay/setup-code 0600 root root - 3939"
):
    fail("development setup code is not restored owner-only on every boot")
if "${nonarch_libdir}/tmpfiles.d/prelude-dev.conf" not in recipe:
    fail(
        "development tmpfiles provisioning is not assigned to the dev-config subpackage"
    )
for obsolete_image in ("prelude-otg-image.bb", "prelude-prod-otg-image.bb"):
    if (ROOT / "meta-prelude/recipes-core/images" / obsolete_image).exists():
        fail(f"obsolete image recipe remains: {obsolete_image}")
for image in (otg_image, prod_otg_image):
    if "prelude-topology-otg" not in image:
        fail("projection image does not install the direct-OTG topology")
if "d /var/lib/prelude/airplay 0700 root root -" not in tmpfiles:
    fail("AirPlay identity and pairing state directory is not owner-only")
if "d /var/lib/prelude/diagnostics 0750 root root -" not in tmpfiles:
    fail("persistent diagnostics directory is not created by tmpfiles")
for environment in (
    "XDG_RUNTIME_DIR=/run/weston",
    "WAYLAND_DISPLAY=wayland-1",
):
    if environment not in daemon_env:
        fail(
            f"daemon environment does not target the BSP Weston session: {environment}"
        )
if "profile.release.strip=false" not in recipe:
    fail("Rust recipe lets Cargo strip binaries before Yocto can split debug data")
if "--features preluded/linux-media" not in recipe:
    fail("target daemon is not built through its public Linux media feature")
if "--features prelude-media/linux-backend" in recipe:
    fail("target recipe bypasses the daemon's public media feature")
for build_dependency in ("gstreamer1.0", "gstreamer1.0-plugins-base"):
    if build_dependency not in recipe:
        fail(
            f"Rust target recipe does not stage {build_dependency} development metadata"
        )
if (
    'DEBUG_PREFIX_MAP:append = " -ffile-prefix-map=${WORKDIR}=${TARGET_DBGSRC_DIR}"'
    not in recipe
):
    fail("Rust C dependencies can leak TMPDIR paths into split debug packages")
if "${nonarch_libdir}/tmpfiles.d/prelude.conf" not in recipe:
    fail("Rust recipe does not package its tmpfiles configuration")
if 'PACKAGE_ARCH = "${MACHINE_ARCH}"' not in packagegroup:
    fail("core packagegroup cannot safely depend on ABI-renamed packages as allarch")
if "cursor-suppress" not in packagegroup:
    fail("core packagegroup omits the module loaded unconditionally by weston.ini")
if "prelude_validate_weston_modules" not in image_base:
    fail("image rootfs does not validate Weston module/config coupling")
if (
    "for module in cursor-suppress.so systemd-notify.so; do" not in image_base
    or "${IMAGE_ROOTFS}${libdir}/weston/$module" not in image_base
):
    fail("image rootfs does not assert the Weston module paths")
weston_init = text("meta-prelude/recipes-graphics/weston-init/superbird-weston-init_0.1.bbappend")
if "modules=cursor-suppress.so,systemd-notify.so" not in weston_init:
    fail("Weston does not load systemd-notify.so, so its readiness cannot gate preluded")
if "Type=notify" not in text("meta-prelude/recipes-graphics/weston-init/files/weston-notify.conf"):
    fail("superbird-weston.service does not wait for the Wayland socket")
if "prelude_validate_host_trial_boot_unit" not in image_base:
    fail("image rootfs does not validate next-boot host-trial service installation")
if "multi-user.target.wants/prelude-host-trial-next-boot.service" not in image_base:
    fail("image rootfs does not validate next-boot host-trial service enablement")
for reference in (
    "file://prelude-host-test",
    "file://prelude-host-test-display",
    "prelude-host-test.service",
    "prelude-host-test-display.service",
    "file://preluded-host-test.conf",
):
    if reference not in host_test_recipe:
        fail(f"USB host-test recipe is missing {reference}")
if "ExecStart=/usr/libexec/prelude-host-test" not in host_test_unit:
    fail("USB host-test unit does not execute the bounded runner")
if "OnFailure=prelude-device-recovery.service" not in host_test_unit:
    fail("USB host-test failure is not connected to device recovery")
if "TimeoutStartSec=180s" not in host_test_unit:
    fail("USB host-test service has no bounded execution deadline")
if "Before=preluded.service" not in host_test_unit:
    fail("projection can race the USB host qualification runner")
if "weston-terminal --fullscreen" not in host_test_display_unit:
    fail("USB host-test image has no full-screen status display")
for safety_contract in (
    'preludectl host-trial --timeout "$trial_seconds"',
    "trial_seconds=150",
    "observation_seconds=60",
    "preludectl device-mode",
    "external_devices",
    "FAIL: ROOT HUB ONLY",
    "/var/lib/prelude/diagnostics/usb-host-test",
    "/proc/sys/kernel/random/boot_id",
    "diagnostics_root/runs/$run_id",
    '"$diagnostics_root/latest-run"',
):
    if safety_contract not in host_test_runner:
        fail(f"USB host-test runner is missing its safety contract: {safety_contract}")
if "host-confirm" in host_test_runner:
    fail("USB host-test runner can persist host mode")
if "prelude-host-test" not in host_test_image:
    fail("USB host-test image does not install the qualification package")
if "preluded.service" not in host_test_image:
    fail("USB host-test image does not disable preluded.service")
dropin = ROOT / (
    "meta-prelude/recipes-core/prelude-host-test/files/preluded-host-test.conf"
)
if "ConditionPathExists=!/usr/libexec/prelude-host-test" not in dropin.read_text():
    fail("USB host-test image does not inhibit preluded.service at runtime")
if "prelude_apply_host_test_policy" not in host_test_image:
    fail("USB host-test rootfs does not assert its service policy")
if 'PACKAGES = "${PN}-otg"' not in topology_recipe:
    fail("topology recipe does not define only the direct-OTG package")
for setting in (
    "disable_audio_output = false",
    'video = "amlogic_v4l2"',
    "configuration_reconnect_timeout_ms = 15000",
    "mux_io_timeout_ms = 5000",
    "display_width_physical_mm = 86",
    "display_height_physical_mm = 52",
    "display_max_fps = 60",
):
    if setting not in config_dev:
        fail(f"direct-OTG daemon configuration is missing {setting}")
for source in (
    "file://prelude-session-recover",
    "file://prelude-otg-recovery.service",
    "file://prelude-otg-recovery.path",
    "file://otg-always-host.conf",
    "file://otg-host-health.conf",
):
    if source not in topology_recipe:
        fail(f"topology recipe omits recovery payload {source}")
if 'SYSTEMD_PACKAGES = "${PN}-otg"' not in topology_recipe:
    fail("session recovery units are not assigned to the direct-OTG package")
for command in (
    '"$systemctl_bin" stop preluded.service',
    'mv -f "$failure" "$blocked_failure"',
    '"$systemctl_bin" start preluded.service',
):
    if command not in session_recovery_script:
        fail(f"session recovery script is missing {command}")
if (
    "PathExists=/var/lib/prelude/diagnostics/last-wired-failure"
    not in otg_recovery_path
):
    fail("OTG recovery path does not watch the durable daemon failure record")
if "ExecStart=/usr/libexec/prelude-session-recover" not in otg_recovery_service:
    fail("OTG recovery service does not use the bounded recovery helper")
if "prelude-topology-otg" not in otg_recovery_service:
    fail("OTG recovery service is not restricted to the direct topology")
if (
    "Restart=on-failure" in otg_recovery_service
    or "TimeoutStartSec=90s" not in otg_recovery_service
):
    fail("OTG recovery service can loop transport recovery indefinitely")
for contract in (
    "ConditionPathExists=\n",
    "ConditionPathExists=/dev/i2c-2",
    "ConditionPathExists=/usr/libexec/prelude-topology-otg",
    "ExecStart=\n",
    "ExecStart=/usr/bin/preludectl automatic-host-boot",
):
    if contract not in otg_always_host:
        fail(f"OTG automatic-host override is missing {contract!r}")
if otg_host_health.count("ConditionPathExists=\n") != 1:
    fail("OTG host-health override does not reset mutable confirmation gating")
if "ConditionPathExists=/usr/libexec/prelude-topology-otg" not in otg_host_health:
    fail("OTG host-health override is not gated by the immutable topology marker")
for owned_path in (
    "prelude-usb-host.service.d/20-otg-always-host.conf",
    "prelude-host-health.service.d/20-otg-always-host.conf",
    "prelude-host-health-failure.service.d/20-otg-always-host.conf",
):
    if owned_path not in topology_recipe:
        fail(f"OTG topology package does not own {owned_path}")
for contract in (
    "prelude-topology-otg",
    "config-dev.toml",
    "prelude-session-recover",
    "prelude-otg-recovery.service",
    "prelude-otg-recovery.path",
    "prelude-usb-host.service",
    "prelude-host-trial-next-boot.service",
    "prelude-device-recovery.path",
    "20-otg-always-host.conf",
    "automatic-host-boot",
    "prelude_validate_otg_topology",
):
    if contract not in otg_image:
        fail(f"OTG image is missing its topology contract: {contract}")
for contract in (
    "prelude-topology-otg",
    "config-dev.toml",
    "prelude-runtime-dev-config",
    "superbird-usb-gadget",
    "android-tools-adbd",
    "preluded.service.d/10-dev.conf",
    "prelude-usb-host.service",
    "20-otg-always-host.conf",
    "prelude_disable_prod_otg_recovery_boot_units",
    "00-prelude-prod-otg-recovery.preset",
    "disable superbird-usb-gadget.service",
    "disable android-tools-adbd.service",
    'rm -f "$wants/$unit"',
    'ROOTFS_POSTPROCESS_COMMAND:append = " prelude_disable_prod_otg_recovery_boot_units;',
    "prelude_validate_prod_otg_topology",
    'mode = "automatic"',
    'audio = "alsa_usb"',
    'microphone = "built_in_superbird"',
):
    if contract not in prod_otg_image:
        fail(f"production OTG image is missing its topology contract: {contract}")
if "SYSTEMD_AUTO_ENABLE" in prod_otg_image:
    fail(
        "production OTG image incorrectly tries to override another recipe's systemd policy"
    )
for label, logo_overlay in logo_overlays.items():
    if not logo_overlay.is_file() or logo_overlay.stat().st_size != 800 * 480 * 3 // 2:
        fail(f"packaged {label} logo is not one 800x480 NV12 frame")
expected_logo_hashes = {
    "CarPlay": "036c4d51198eaf0fbb2fd13c3d3b6cfab1eaff5b44ecf8c5d17ea5267075b124",
    "Android Auto": "5c658f778f8265a02913e6c77d684d542fe95951225f33e53019f3ae3ae3fb52",
}
for label, logo_overlay in logo_overlays.items():
    if hashlib.sha256(logo_overlay.read_bytes()).hexdigest() != expected_logo_hashes[label]:
        fail(f"packaged {label} logo does not match the supplied frame")
    if expected_logo_hashes[label] not in prod_otg_image:
        fail(f"production OTG image does not pin the {label} logo frame")
if "Inter-Regular.ttf" not in prod_otg_image:
    fail("production OTG image does not check for the status text font")
for package_contract in (
    "file://carplay-logo.nv12",
    "file://android-auto-logo.nv12",
    "$overlay is not an 800x480 NV12 frame",
    "036c4d51198eaf0fbb2fd13c3d3b6cfab1eaff5b44ecf8c5d17ea5267075b124",
    "5c658f778f8265a02913e6c77d684d542fe95951225f33e53019f3ae3ae3fb52",
    "${datadir}/prelude/*.nv12",
):
    if package_contract not in recipe:
        fail(f"runtime package is missing startup overlay contract: {package_contract}")
for unsupported_path in (
    "prelude-usbip-client",
    "prelude-usbip-client.service",
    "${sbindir}/usbip",
):
    if unsupported_path not in otg_image:
        fail(
            f"OTG image does not reject unsupported USB/IP payload: {unsupported_path}"
        )
    if unsupported_path not in prod_otg_image:
        fail(
            "production OTG image does not reject unsupported USB/IP payload: "
            f"{unsupported_path}"
        )
for unsupported_package in (
    "usbip-tools",
    "kernel-module-usbip-core",
    "kernel-module-vhci-hcd",
):
    if unsupported_package in packagegroup or unsupported_package in recipe:
        fail(f"runtime packages unsupported USB/IP dependency: {unsupported_package}")
if "iproute2" not in packagegroup:
    fail("core packagegroup lacks the fixed-path ip utility required by NCM link setup")
# Wi-Fi client mode drives wpa_supplicant and udhcpc with Prelude's hook.
# wpa-supplicant stays a BAD_RECOMMENDATION, so only an explicit RDEPENDS
# installs it.
if not re.search(r"^\s+wpa-supplicant \\$", packagegroup, re.MULTILINE):
    fail("core packagegroup must depend on wpa-supplicant for Wi-Fi client mode")
for needle in (
    "file://prelude-udhcpc-script",
    "install -m 0755 ${UNPACKDIR}/prelude-udhcpc-script ${D}${libexecdir}/prelude-udhcpc-script",
    "${libexecdir}/prelude-udhcpc-script",
):
    if needle not in recipe:
        fail(f"runtime recipe must ship the Wi-Fi client DHCP hook ({needle})")
if 'LICENSE_FLAGS_ACCEPTED += "commercial_faad2"' not in distro:
    fail("distro does not narrowly accept FAAD2's restricted-policy flag")
if "commercial_ffmpeg" in distro or "commercial_gstreamer1.0-libav" in distro:
    fail("hardware-only images must not accept software H.264 decoder license flags")
if "gstreamer1.0-libav" in topology_recipe:
    fail("the direct-OTG topology must not install a software H.264 decoder")
if 'PACKAGECONFIG:append = " opus"' not in gst_base_policy:
    fail("GStreamer base plugins are missing the Opus decoder")
if 'PACKAGECONFIG:append = " faad"' not in gst_bad_policy:
    fail("GStreamer bad plugins are missing the FAAD AAC decoder")
if "MACHINE_FEATURES:remove" in distro:
    fail("distro removes BSP machine features that wireless projection needs")
distro_removals = re.search(
    r'^DISTRO_FEATURES:remove\s*=\s*"([^"]*)"', distro, re.MULTILINE
)
if distro_removals is None:
    fail("distro does not trim unused BSP distro features")
for radio_feature in ("wifi", "bluetooth"):
    if radio_feature in distro_removals.group(1).split():
        fail(f"distro removes the {radio_feature} feature that wireless projection needs")
for obsolete_toggle in ("PRELUDE_WIRELESS", "prelude-wireless"):
    if obsolete_toggle in distro:
        fail(f"distro retains the obsolete {obsolete_toggle} radio toggle")
if obsolete_bluealsa_policy.exists():
    fail("obsolete BlueALSA recipe customization remains in the downstream layer")
if "bluealsa" in packagegroup:
    fail("core packagegroup directly installs bluealsa")
if 'PACKAGE_EXCLUDE += "bluealsa"' not in image_base:
    fail("rootfs package exclusion omits bluealsa")
for forbidden_dev_package in ("bluez5-noinst-tools", "bluealsa"):
    if forbidden_dev_package in dev_packagegroup:
        fail(f"development packagegroup installs {forbidden_dev_package}")
if 'RDEPENDS:${PN}:remove = "superbird-usb-gadget"' not in bsp_packagegroup:
    fail("BSP runtime override does not remove only the USB gadget")
if 'FILESEXTRAPATHS:prepend := "${THISDIR}/files:"' not in superbird_init_policy:
    fail("Superbird init does not select the downstream payload")
for init_payload in (
    superbird_init_policy,
    superbird_init_script,
    superbird_init_unit,
    superbird_metadata_template,
):
    if re.search(r"(?i)bluetooth|bluez|bluealsa", init_payload):
        fail("downstream Superbird init payload retains radio behavior")
if "serial-number@*" not in superbird_init_script:
    fail("downstream Superbird init no longer preserves the device serial")
if '"serialNumber": ""' not in superbird_metadata_template:
    fail("downstream Superbird metadata template omits the device serial field")
expected_media = {
    config_dev: {
        'video = "amlogic_v4l2"',
        'audio = "alsa_usb"',
        'microphone = "built_in_superbird"',
    },
    config: {
        'video = "none"',
        'audio = "none"',
        'microphone = "none"',
    },
}
for packaged_config, expected_settings in expected_media.items():
    media_match = re.search(r"(?ms)^\[media\]\s*$(.*?)(?=^\[|\Z)", packaged_config)
    if media_match is None:
        fail("packaged configuration has no media section")
    media_settings = {
        line.strip()
        for line in media_match.group(1).splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    if media_settings != expected_settings:
        fail("packaged media configuration guesses hardware identity or changes routes")
if "[media.usb_dac]" in config:
    fail("production configuration embeds development USB DAC identity")
dac_match = re.search(r"(?ms)^\[media\.usb_dac\]\s*$(.*?)(?=^\[|\Z)", config_dev)
if dac_match is None:
    fail("development configuration does not enable USB DAC discovery")
dac_settings = [
    line.strip()
    for line in dac_match.group(1).splitlines()
    if line.strip() and not line.lstrip().startswith("#")
]
if dac_settings:
    fail("development configuration pins USB DAC hardware instead of discovering it")
for packaged_config in (config, config_dev):
    if "preferred_audio_route" in packaged_config:
        fail(
            "packaged daemon configuration contains removed preferred_audio_route field"
        )
    if re.search(r"(?i)bluetooth|bluez|bluealsa|a2dp|hfp|rfcomm", packaged_config):
        fail("packaged daemon configuration retains a removed radio setting")
for media_package in (
    "gstreamer1.0-plugins-base-alsa",
    "gstreamer1.0-plugins-base-app",
    "gstreamer1.0-plugins-base-audioconvert",
    "gstreamer1.0-plugins-base-audiomixer",
    "gstreamer1.0-plugins-base-audioresample",
    "gstreamer1.0-plugins-base-audiotestsrc",
    "gstreamer1.0-plugins-base-opus",
    "gstreamer1.0-plugins-base-volume",
    "gstreamer1.0-plugins-base-videoconvertscale",
    "gstreamer1.0-plugins-good-audioparsers",
    "gstreamer1.0-plugins-good-equalizer",
    "gstreamer1.0-plugins-good-rtp",
    "gstreamer1.0-plugins-good-rtpmanager",
    "gstreamer1.0-plugins-good-video4linux2",
    "gstreamer1.0-plugins-bad-faad",
    "gstreamer1.0-plugins-bad-videoparsersbad",
):
    if media_package not in packagegroup:
        fail(f"core packagegroup does not install {media_package}")
for excluded_media_package in (
    "gstreamer1.0-plugins-bad-waylandsink",
    "gstreamer1.0-plugins-bad-v4l2codecs",
):
    if excluded_media_package in packagegroup:
        fail(f"unselected media path unexpectedly installs {excluded_media_package}")
if "superbird-usb-gadget" not in dev_packagegroup:
    fail("development image has no direct-USB gadget recovery package")
if "superbird-usb-gadget" not in bsp_packagegroup:
    fail("production packagegroup still pulls in the USB gadget")
for name, policy in (("USB gadget", gadget_policy), ("adbd", adbd_policy)):
    enabled = any(
        setting in policy
        for setting in (
            'SYSTEMD_AUTO_ENABLE:${PN} = "enable"',
            'SYSTEMD_AUTO_ENABLE:${PN}-adbd = "enable"',
        )
    )
    if not enabled:
        fail(f"development {name} recovery service is not boot-enabled")
if "/usr/bin/preludectl device-mode" not in role_helper:
    fail("BSP USB-role timeout does not return through the serialized controller")
if "PRELUDE_ROLE_CONTROLLER" not in role_helper:
    fail("BSP USB-role helper does not reject unmediated role mutations")
if uboot_append.exists() or uboot_probe.exists():
    fail("downstream U-Boot must remain on the pinned upstream implementation")
if "OnFailure=prelude-host-health-failure.service" not in health_unit:
    fail("host-health failures are not wired to the bounded recovery policy")
if not all(
    setting in role_policy
    for setting in (
        'SYSTEMD_AUTO_ENABLE = "disable"',
        'SYSTEMD_AUTO_ENABLE:${PN} = "disable"',
    )
):
    fail("legacy BSP USB-role persistence unit remains boot-enabled")
for module in (
    "snd-usb-audio",
    "usbnet",
    "cdc-ncm",
    "ax88179-178a",
    "r8152",
    "ipheth",
    "tun",
):
    if f"kernel-module-{module}" not in packagegroup:
        fail(f"core packagegroup does not install kernel-module-{module}")
for setting in (
    "CONFIG_USB=y",
    "CONFIG_USB_XHCI_HCD=y",
    "CONFIG_USB_XHCI_PLATFORM=y",
    "CONFIG_USB_DWC3=y",
    "CONFIG_USB_DWC3_MESON_G12A=y",
    "CONFIG_USB_ROLE_SWITCH=y",
    "CONFIG_USB_DWC2=y",
    "CONFIG_USB_DWC2_DUAL_ROLE=y",
    "CONFIG_PHY_MESON_G12A_USB2=y",
    "CONFIG_USB_IPHETH=m",
    "CONFIG_TUN=m",
    "# CONFIG_USBIP_CORE is not set",
    "# CONFIG_USBIP_VHCI_HCD is not set",
):
    if setting not in kernel_config:
        fail(f"kernel fragment is missing {setting}")
if "CONFIG_BT is not set" in kernel_config:
    fail("USB kernel fragment disables the Bluetooth stack wireless projection needs")
for fragment in ("file://prelude-usb.cfg", "file://prelude-wireless.cfg"):
    if fragment not in kernel_policy:
        fail(f"downstream Linux recipe does not apply {fragment}")
for radio_removal in ("SRC_URI:remove", "uart_A"):
    if radio_removal in kernel_policy:
        fail(f"downstream Linux recipe still strips the onboard radio: {radio_removal}")
for update_recipe, flavour, source_image, artifact in (
    (dev_update_recipe, "dev", "prelude-dev-image", "squashfs-lz4"),
    (prod_update_recipe, "prod", "prelude-prod-image", "ext4"),
):
    for update_contract in (
        "require recipes-core/superbird-bsp-update/superbird-bsp-update.inc",
        "require prelude-update-signing.inc",
        f'PRELUDE_UPDATE_FLAVOUR = "{flavour}"',
        f'SUPERBIRD_OTA_SOURCE_IMAGE = "{source_image}"',
        f'SUPERBIRD_OTA_SYSTEM_ARTIFACT = "${{SUPERBIRD_OTA_SOURCE_LINKNAME}}.{artifact}"',
        'SUPERBIRD_OTA_SYSTEM_CPIO_NAME = "system.img"',
    ):
        if update_contract not in update_recipe:
            fail(f"{flavour} A/B update recipe is missing contract: {update_contract}")
for signing_contract in (
    'SWUPDATE_SIGNING = "RSA"',
    'SWUPDATE_PRIVATE_KEY = "${PRELUDE_OTA_SIGNING_KEY}"',
    'IMAGE_LINK_NAME = "prelude_update_${PRELUDE_UPDATE_FLAVOUR}_v${DISTRO_VERSION}"',
    'do_swuimage[prefuncs] += "prelude_require_signing_key prelude_check_root_fits"',
):
    if signing_contract not in update_signing:
        fail(f"OTA bundle signing is missing contract: {signing_contract}")
if re.search(r"^python __anonymous|^python \(\)", update_signing, re.MULTILINE):
    fail("OTA key checks must not run at parse time; keyless image builds would break")
for image_name in ("boot.vfat", "system.img"):
    if f'sha256 = "$swupdate_get_sha256({image_name})";' not in update_sw_description:
        fail(f"signed sw-description does not hash {image_name}")
swupdate_signing = text("meta-prelude/recipes-support/swupdate/files/prelude-signing.cfg")
for option in ("CONFIG_SIGNED_IMAGES=y", "CONFIG_SIGALG_RAWRSA=y"):
    if option not in swupdate_signing:
        fail(f"swupdate is not built with {option}")
kas_run = text("scripts/kas-run")
for variable in ("PRELUDE_OTA_SIGNING_KEY", "PRELUDE_OTA_PUBLIC_KEY", "PRELUDE_OTA_KEY_DIR"):
    if variable not in kas_run:
        fail(f"kas-run does not pass {variable} into the build container")
for variable in ("PRELUDE_OTA_SIGNING_KEY", "PRELUDE_OTA_PUBLIC_KEY"):
    if f"  {variable}: null" not in kas:
        fail(f"KAS profile does not pass {variable} to BitBake")

wireless_kernel_config = text(
    "meta-prelude/recipes-kernel/linux/linux-superbird/prelude-wireless.cfg"
)
for setting in (
    "CONFIG_BT=y",
    "CONFIG_BT_RFCOMM=y",
    "CONFIG_BT_HCIUART_BCM=y",
    "CONFIG_CFG80211=m",
    "CONFIG_MAC80211=m",
):
    if setting not in wireless_kernel_config:
        fail(f"wireless kernel fragment is missing {setting}")
if 'ROOTFS_POSTPROCESS_COMMAND += "prelude_validate_wireless_rootfs; "' not in image_base:
    fail("image rootfs does not validate the wireless package policy")
for layer_file in ROOT.glob("meta-prelude/**/*"):
    if layer_file.is_file() and layer_file.suffix in (".bb", ".bbappend", ".inc", ".conf"):
        if re.search(r":prelude-wireless\b", layer_file.read_text(encoding="utf-8")):
            fail(f"{layer_file.relative_to(ROOT)} still keys off the removed prelude-wireless override")
for wireless_package in ("hostapd", "dnsmasq", "iw", "wireless-regdb-static"):
    if wireless_package not in packagegroup:
        fail(f"wireless packagegroup does not install {wireless_package}")
if "preluded-wireless.conf" not in recipe or "30-wireless.conf" not in recipe:
    fail("wireless builds do not pull in Bluetooth for preluded")
wireless_dropin = text("meta-prelude/recipes-core/prelude/files/preluded-wireless.conf")
if "Wants=bluetooth.service" not in wireless_dropin:
    fail("wireless builds do not pull in Bluetooth for preluded")
if re.search(r"(?m)^After=.*\b(superbird-)?bluetooth\.service", wireless_dropin):
    fail("preluded must not wait for the Bluetooth patchram download before showing the display")
if "20-prelude-persistent.conf" not in recipe or "Storage=persistent" not in text(
    "meta-prelude/recipes-core/prelude/files/journald-persistent.conf"
):
    fail("wireless builds lose the journal of the boot that failed")
if "rootwait quiet systemd.show_status=error" not in text(
    "meta-prelude/recipes-bsp/superbird-extlinux/superbird-extlinux.bbappend"
):
    fail("kernel command line writes the full boot log to the 115200 baud console")
if "hfp-profiles" not in text("meta-prelude/recipes-connectivity/bluez5/bluez5_%.bbappend"):
    fail("BlueZ native HFP would race the preluded Hands-Free profile")
if "JustWorksRepairing = always" not in text("meta-prelude/recipes-connectivity/bluez5/bluez5_%.bbappend"):
    fail("BlueZ would reject phones re-pairing after forgetting the Car Thing")
if 'SYSTEMD_AUTO_ENABLE:${PN} = "disable"' not in text(
    "meta-prelude/recipes-connectivity/dnsmasq/dnsmasq_%.bbappend"
):
    fail("the system dnsmasq instance would conflict with the projection access point")

# OTA: every image records its flavour so the updater picks the matching
# bundle, and images built without a public key ship with OTA disabled.
image_base = text("meta-prelude/recipes-core/images/prelude-image-base.inc")
for image_recipe, flavour in (
    ("prelude-dev-image.bb", "dev"),
    ("prelude-prod-image.bb", "prod"),
):
    recipe_text = text(f"meta-prelude/recipes-core/images/{image_recipe}")
    declared = re.search(r'^PRELUDE_FLAVOUR \??= "(\w+)"', recipe_text, re.MULTILINE)
    inherited = re.search(r'^PRELUDE_FLAVOUR \?\?= "(\w+)"', image_base, re.MULTILINE)
    effective = declared.group(1) if declared else (inherited.group(1) if inherited else None)
    if effective != flavour:
        fail(f"{image_recipe} does not record PRELUDE_FLAVOUR = {flavour}")
if 'PRELUDE_OTA_ENABLED = "0"' not in text(
    "meta-prelude/recipes-core/images/prelude-usb-host-test-image.bb"
):
    fail("the USB host test image must never accept OTA updates")
# swupdate is built with CONFIG_SIGNED_IMAGES and cannot start without a key, so
# images with OTA disabled must not try to start it on every boot.
for unit in ("swupdate.service", "swupdate.socket"):
    if f'ln -sf /dev/null "${{IMAGE_ROOTFS}}${{sysconfdir}}/systemd/system/{unit}"' not in image_base:
        fail(f"images with OTA disabled must mask {unit}")
if "prelude_write_ota_markers" not in image_base or "ota-disabled" not in image_base:
    fail("images do not write the flavour and OTA availability markers")


runtime_recipe = text("meta-prelude/recipes-core/prelude/prelude-runtime_0.1.0.bb")
if 'PRELUDE_UPDATES_REPOSITORY ??= "usenocturne/prelude"' not in runtime_recipe:
    fail("runtime recipe does not default the OTA release repository")
updates_at = runtime_recipe.find("[updates]")
if updates_at < 0 or 'repository = \"%s\"' not in runtime_recipe:
    fail("runtime recipe does not write an [updates] repository")
for config_file in ("config.toml", "config-dev.toml"):
    if f">> ${{D}}${{sysconfdir}}/prelude/{config_file}" not in runtime_recipe[updates_at:]:
        fail(f"runtime recipe does not bake the OTA release repository into {config_file}")
kas_profile = text("kas/prelude.yml")
if "PRELUDE_UPDATES_REPOSITORY: null" not in kas_profile:
    fail("KAS profile does not forward PRELUDE_UPDATES_REPOSITORY")
if "PRELUDE_UPDATES_REPOSITORY" not in text("scripts/kas-run"):
    fail("kas-run does not forward PRELUDE_UPDATES_REPOSITORY")

print("prelude image layer static checks passed")
