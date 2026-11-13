#!/bin/sh
# Exercise the OTA trial-boot watchdog decision table without systemd or U-Boot.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
watchdog="$root/meta-prelude/recipes-core/superbird-slot-ok/files/prelude-trial-watchdog"
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT HUP INT TERM

cat > "$test_dir/fake-systemctl" <<'EOF'
#!/bin/sh
echo "$*" >> "$CT_TEST_SYSTEMCTL_LOG"
EOF
cat > "$test_dir/fake-fw_printenv" <<'EOF'
#!/bin/sh
[ "$1" = "-n" ] && [ "$2" = "slot_active" ] || { echo "unexpected fw_printenv $*" >&2; exit 2; }
printf '%s\n' "$CT_TEST_SLOT_ACTIVE"
EOF
chmod +x "$test_dir/fake-systemctl" "$test_dir/fake-fw_printenv"

export PRELUDE_SYSTEMCTL="$test_dir/fake-systemctl"
export PRELUDE_FW_PRINTENV="$test_dir/fake-fw_printenv"
export PRELUDE_RUN_DIR="$test_dir/run"
export PRELUDE_STATE_DIR="$test_dir/state"
export PRELUDE_CMDLINE="$test_dir/cmdline"
export CT_TEST_SYSTEMCTL_LOG="$test_dir/systemctl.log"

reset() {
    rm -rf "$PRELUDE_RUN_DIR" "$PRELUDE_STATE_DIR" "$CT_TEST_SYSTEMCTL_LOG"
    mkdir -p "$PRELUDE_RUN_DIR" "$PRELUDE_STATE_DIR"
    : > "$CT_TEST_SYSTEMCTL_LOG"
    printf 'console=ttyS0 superbird.slot=%s quiet\n' "$1" > "$PRELUDE_CMDLINE"
    export CT_TEST_SLOT_ACTIVE="$2"
}

pending() {
    printf 'slot=%s\nversion=1.0.5\n' "$1" > "$PRELUDE_STATE_DIR/pending-update"
}

expect_no_reboot() {
    if grep -q reboot "$CT_TEST_SYSTEMCTL_LOG"; then
        echo "FAIL $1: watchdog rebooted" >&2
        exit 1
    fi
}

# ready_present_no_reboot
reset b b; pending b; : > "$PRELUDE_RUN_DIR/ready"
"$watchdog"
expect_no_reboot ready_present_no_reboot
[ -f "$PRELUDE_STATE_DIR/pending-update" ] || { echo "FAIL ready_present_no_reboot: marker removed" >&2; exit 1; }

# recovery_boot_no_reboot
reset b b; pending b; : > "$PRELUDE_RUN_DIR/recovery-boot"
"$watchdog"
expect_no_reboot recovery_boot_no_reboot

# no_pending_no_reboot
reset b b
"$watchdog"
expect_no_reboot no_pending_no_reboot

# stale_marker_cleared_when_never_committed: running a, marker says b, env still a
reset a a; pending b
"$watchdog"
expect_no_reboot stale_marker_cleared_when_never_committed
[ ! -e "$PRELUDE_STATE_DIR/pending-update" ] || { echo "FAIL stale marker kept" >&2; exit 1; }

# marker_cleared_after_fallback: running a after U-Boot fell back, env says b
reset a b; pending b
"$watchdog"
expect_no_reboot marker_cleared_after_fallback
[ ! -e "$PRELUDE_STATE_DIR/pending-update" ] || { echo "FAIL fallback marker kept" >&2; exit 1; }

# pending_this_slot_not_ready_reboots
reset b b; pending b
"$watchdog"
grep -qx reboot "$CT_TEST_SYSTEMCTL_LOG" || { echo "FAIL pending_this_slot_not_ready_reboots: no reboot" >&2; exit 1; }
[ -f "$PRELUDE_STATE_DIR/pending-update" ] || { echo "FAIL marker removed before fallback" >&2; exit 1; }

# missing_slot_on_cmdline_no_reboot
reset b b; pending b; printf 'console=ttyS0 quiet\n' > "$PRELUDE_CMDLINE"
"$watchdog"
expect_no_reboot missing_slot_on_cmdline_no_reboot

echo "prelude trial watchdog checks passed"
