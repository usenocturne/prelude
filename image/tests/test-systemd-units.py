#!/usr/bin/env python3
"""Check the local boot-unit ordering and recovery invariants without systemd."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
UNIT_DIR = ROOT / "meta-prelude/recipes-core/prelude/files"
HOST_TEST_UNIT_DIR = ROOT / "meta-prelude/recipes-core/prelude-host-test/files"
TOPOLOGY_UNIT_DIR = ROOT / "meta-prelude/recipes-core/prelude-topology/files"
SLOT_OK_UNIT_DIR = ROOT / "meta-prelude/recipes-core/superbird-slot-ok/files"


def parse_unit(
    name: str, directory: Path = UNIT_DIR
) -> dict[str, dict[str, list[str]]]:
    sections: dict[str, dict[str, list[str]]] = {}
    section: str | None = None
    for raw_line in (directory / name).read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith(("#", ";")):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
            sections.setdefault(section, {})
            continue
        if section is None or "=" not in line:
            raise AssertionError(f"invalid unit line in {name}: {raw_line}")
        key, value = line.split("=", 1)
        sections[section].setdefault(key, []).append(value)
    return sections


def values(unit: dict[str, dict[str, list[str]]], section: str, key: str) -> list[str]:
    return unit.get(section, {}).get(key, [])


def words(unit: dict[str, dict[str, list[str]]], section: str, key: str) -> set[str]:
    return {word for value in values(unit, section, key) for word in value.split()}


units = {path.name: parse_unit(path.name) for path in UNIT_DIR.glob("*.service")}
recovery_path = parse_unit("prelude-device-recovery.path")
host_test = parse_unit("prelude-host-test.service", HOST_TEST_UNIT_DIR)
host_test_display = parse_unit(
    "prelude-host-test-display.service", HOST_TEST_UNIT_DIR
)
otg_session_recovery = parse_unit("prelude-otg-recovery.service", TOPOLOGY_UNIT_DIR)
otg_session_recovery_path = parse_unit(
    "prelude-otg-recovery.path", TOPOLOGY_UNIT_DIR
)
otg_always_host = parse_unit("otg-always-host.conf", TOPOLOGY_UNIT_DIR)
otg_host_health = parse_unit("otg-host-health.conf", TOPOLOGY_UNIT_DIR)

trial_name = "prelude-host-trial-next-boot.service"
host_name = "prelude-usb-host.service"
daemon_name = "preluded.service"
health_name = "prelude-host-health.service"
recovery_name = "prelude-device-recovery.service"
trial = units[trial_name]

assert values(trial, "Service", "Type") == ["oneshot"]
assert values(trial, "Service", "ExecStart") == [
    "/usr/bin/preludectl consume-host-trial-next-boot"
]
assert values(trial, "Service", "TimeoutStartSec") == ["90s"]
assert set(values(trial, "Unit", "ConditionPathExists")) == {
    "/dev/i2c-2",
    "/var/lib/prelude/host-trial-next-boot",
}
assert words(trial, "Unit", "After") >= {
    "local-fs.target",
    "systemd-modules-load.service",
    "systemd-udev-trigger.service",
    "superbird-usb-gadget.service",
    "android-tools-adbd.service",
    host_name,
}
assert words(trial, "Unit", "Before") == {daemon_name}
assert words(trial, "Unit", "OnFailure") == {recovery_name}
assert words(trial, "Install", "WantedBy") == {"multi-user.target"}

assert trial_name in words(units[daemon_name], "Unit", "After")
assert words(units[daemon_name], "Unit", "Conflicts") == {"superbird-als.service"}
assert values(units[daemon_name], "Unit", "StartLimitIntervalSec") == ["0"]
assert values(units[daemon_name], "Service", "Restart") == ["on-failure"]
assert values(units[daemon_name], "Service", "RestartSec") == ["2s"]
assert set(values(units[daemon_name], "Service", "ReadWritePaths")) >= {
    "/run/prelude /var/lib/prelude",
    "-/sys/bus/iio/devices -/sys/class/backlight",
}
backlight = units["prelude-backlight.service"]
assert values(backlight, "Unit", "DefaultDependencies") == ["no"]
assert values(backlight, "Service", "Type") == ["oneshot"]
assert values(backlight, "Service", "ExecStart") == [
    "/usr/bin/preludectl initial-backlight"
]
assert words(backlight, "Unit", "After") == {"local-fs.target"}
assert words(backlight, "Unit", "Before") >= {"sysinit.target", daemon_name}
assert words(backlight, "Unit", "Conflicts") == {"shutdown.target"}
assert words(backlight, "Install", "WantedBy") == {"sysinit.target"}

assert daemon_name in words(units[host_name], "Unit", "Before")
assert host_name in words(units[health_name], "Unit", "Requires")
assert "/var/lib/prelude/persistent-host-enabled" in values(
    units[health_name], "Unit", "ConditionPathExists"
)
assert trial_name not in words(units[recovery_name], "Unit", "After")
assert values(units[recovery_name], "Unit", "StartLimitIntervalSec") == ["0"]
assert values(units[recovery_name], "Service", "ExecStartPre") == ["/bin/sleep 5"]
assert values(units[recovery_name], "Service", "Restart") == ["on-failure"]
assert values(units[recovery_name], "Service", "RestartSec") == ["60s"]
assert values(units[recovery_name], "Service", "TimeoutStartSec") == ["15s"]
assert values(recovery_path, "Path", "PathExists") == [
    "/var/lib/prelude/device-recovery-required"
]
assert values(recovery_path, "Path", "Unit") == [recovery_name]

assert values(otg_always_host, "Unit", "ConditionPathExists") == [
    "",
    "/dev/i2c-2",
    "/usr/libexec/prelude-topology-otg",
]
assert values(otg_always_host, "Service", "ExecStart") == [
    "",
    "/usr/bin/preludectl automatic-host-boot",
]
assert values(otg_host_health, "Unit", "ConditionPathExists") == [
    "",
    "/usr/libexec/prelude-topology-otg",
]

assert values(host_test, "Service", "Type") == ["oneshot"]
assert values(host_test, "Service", "ExecStart") == [
    "/usr/libexec/prelude-host-test"
]
assert values(host_test, "Service", "TimeoutStartSec") == ["180s"]
assert words(host_test, "Unit", "After") >= {
    "local-fs.target",
    "systemd-modules-load.service",
    "systemd-udev-trigger.service",
    "superbird-usb-gadget.service",
    "android-tools-adbd.service",
    "prelude-host-test-display.service",
}
assert words(host_test, "Unit", "Before") == {
    "preluded.service",
}
assert words(host_test, "Unit", "OnFailure") == {recovery_name}
assert words(host_test, "Install", "WantedBy") == {"multi-user.target"}

assert values(host_test_display, "Service", "Type") == ["simple"]
assert values(host_test_display, "Service", "ExecStart") == [
    "/usr/bin/weston-terminal --fullscreen --font-size=28 "
    "--shell=/usr/libexec/prelude-host-test-display"
]
assert words(host_test_display, "Unit", "After") == {"superbird-weston.service"}
assert words(host_test_display, "Unit", "Before") == {"prelude-host-test.service"}

assert values(otg_session_recovery, "Service", "Type") == ["oneshot"]
assert values(otg_session_recovery, "Service", "ExecStart") == [
    "/usr/libexec/prelude-session-recover"
]
assert values(otg_session_recovery, "Service", "TimeoutStartSec") == ["90s"]
assert values(otg_session_recovery, "Service", "Environment") == [
    "PRELUDE_SESSION_TOPOLOGY_MARKER=/usr/libexec/prelude-topology-otg"
]
assert values(otg_session_recovery_path, "Path", "PathExists") == [
    "/var/lib/prelude/diagnostics/last-wired-failure"
]
assert values(otg_session_recovery_path, "Path", "Unit") == [
    "prelude-otg-recovery.service"
]

# Model local After/Before edges and reject dependency cycles introduced by a
# future unit edit. External BSP/systemd units are leaves and need no parsing.
edges = {name: set() for name in units}
for name, unit in units.items():
    for dependency in words(unit, "Unit", "After"):
        if dependency in units:
            edges[dependency].add(name)
    for dependency in words(unit, "Unit", "Before"):
        if dependency in units:
            edges[name].add(dependency)

visiting: set[str] = set()
visited: set[str] = set()


def visit(name: str) -> None:
    if name in visiting:
        raise AssertionError(f"systemd ordering cycle reaches {name}")
    if name in visited:
        return
    visiting.add(name)
    for successor in edges[name]:
        visit(successor)
    visiting.remove(name)
    visited.add(name)


for unit_name in units:
    visit(unit_name)

# OTA health gate: the slot is marked good only once preluded is ready, exactly
# once per boot, and a failed trial boot is rebooted so U-Boot can fall back.
slot_ok_path = parse_unit("prelude-slot-ok.path", SLOT_OK_UNIT_DIR)
assert values(slot_ok_path, "Path", "PathExists") == ["/run/prelude/ready"]
assert values(slot_ok_path, "Path", "Unit") == ["superbird-slot-ok.service"]
assert "multi-user.target" in words(slot_ok_path, "Install", "WantedBy")
slot_ok_dropin = parse_unit("prelude-slot-ok.conf", SLOT_OK_UNIT_DIR)
assert values(slot_ok_dropin, "Service", "RemainAfterExit") == ["yes"]
assert values(slot_ok_dropin, "Service", "ExecStartPost") == [
    "/bin/rm -f /var/lib/prelude/pending-update"
]
watchdog_timer = parse_unit("prelude-trial-watchdog.timer", SLOT_OK_UNIT_DIR)
assert values(watchdog_timer, "Timer", "OnBootSec") == ["5min"]
assert values(watchdog_timer, "Timer", "Unit") == ["prelude-trial-watchdog.service"]
watchdog_service = parse_unit("prelude-trial-watchdog.service", SLOT_OK_UNIT_DIR)
assert values(watchdog_service, "Unit", "ConditionPathExists") == [
    "/var/lib/prelude/pending-update"
]
assert values(watchdog_service, "Service", "ExecStart") == [
    "/usr/libexec/prelude-trial-watchdog"
]
slot_ok_append = (
    ROOT / "meta-prelude/recipes-core/superbird-slot-ok/superbird-slot-ok.bbappend"
).read_text(encoding="utf-8")
assert "superbird-slot-ok.timer" not in slot_ok_append.split("SYSTEMD_SERVICE:${PN} =", 1)[1].splitlines()[0]
assert "ln -sf /dev/null ${D}${sysconfdir}/systemd/system/superbird-slot-ok.timer" in slot_ok_append
# Boots that are not an OTA trial (including images without preluded) are
# still marked good by time, as the BSP did.
time_timer = parse_unit("prelude-slot-ok-time.timer", SLOT_OK_UNIT_DIR)
assert values(time_timer, "Timer", "OnBootSec") == ["60s"]
assert values(time_timer, "Timer", "Unit") == ["prelude-slot-ok-time.service"]
time_service = parse_unit("prelude-slot-ok-time.service", SLOT_OK_UNIT_DIR)
assert values(time_service, "Service", "ExecCondition") == ["/usr/libexec/prelude-trial-watchdog not-trial"]
assert values(time_service, "Service", "ExecStart") == ["/bin/systemctl --no-block start superbird-slot-ok.service"]
assert "prelude-slot-ok-time.timer" in slot_ok_append.split("SYSTEMD_SERVICE:${PN} =", 1)[1].splitlines()[0]

print("prelude systemd unit checks passed")
