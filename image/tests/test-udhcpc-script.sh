#!/bin/sh
# Exercise the Wi-Fi client DHCP hook with fake ip and resolvectl.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
hook="$root/meta-prelude/recipes-core/prelude/files/prelude-udhcpc-script"
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT HUP INT TERM

mkdir -p "$test_dir/bin"
for tool in ip resolvectl; do
    cat > "$test_dir/bin/$tool" <<FAKE
#!/bin/sh
echo "$tool \$*" >> "\$CT_TEST_LOG"
FAKE
    chmod +x "$test_dir/bin/$tool"
done
export PATH="$test_dir/bin:$PATH"
export PRELUDE_CLIENT_DIR="$test_dir/client"
export CT_TEST_LOG="$test_dir/log"
lease="$PRELUDE_CLIENT_DIR/lease"

reset() {
    rm -rf "$PRELUDE_CLIENT_DIR" "$CT_TEST_LOG"
    mkdir -p "$PRELUDE_CLIENT_DIR"
    : > "$CT_TEST_LOG"
    unset ip mask router dns 2>/dev/null || true
    export interface=wlan0
}

fail() {
    echo "FAIL $1: $2" >&2
    echo "--- log" >&2; cat "$CT_TEST_LOG" >&2
    exit 1
}

logged() {
    grep -qxF -- "$2" "$CT_TEST_LOG" || fail "$1" "missing '$2'"
}

not_logged() {
    if grep -q -- "$2" "$CT_TEST_LOG"; then fail "$1" "unexpected '$2'"; fi
}

full_lease() {
    export ip=192.168.1.23 mask=24 router="192.168.1.1 192.168.1.2" dns="1.1.1.1 8.8.8.8"
}

expect_full_lease() {
    logged "$1" "ip addr flush dev wlan0"
    logged "$1" "ip addr add 192.168.1.23/24 dev wlan0"
    logged "$1" "ip route replace default via 192.168.1.1 dev wlan0"
    logged "$1" "resolvectl dns wlan0 1.1.1.1 8.8.8.8"
    logged "$1" "resolvectl domain wlan0 ~."
    expected=$(printf 'address=192.168.1.23/24\nrouter=192.168.1.1\ndns=1.1.1.1 8.8.8.8\n')
    [ "$(cat "$lease")" = "$expected" ] || fail "$1" "lease file: $(cat "$lease")"
}

# bound_applies_lease
reset; full_lease
"$hook" bound
expect_full_lease bound_applies_lease

# renew_reapplies
reset; full_lease
"$hook" renew
expect_full_lease renew_reapplies

# deconfig_clears
reset; printf 'address=x\n' > "$lease"
"$hook" deconfig
logged deconfig_clears "ip addr flush dev wlan0"
logged deconfig_clears "resolvectl revert wlan0"
[ ! -e "$lease" ] || fail deconfig_clears "lease file kept"

# leasefail_clears
reset; printf 'address=x\n' > "$lease"
"$hook" leasefail
[ ! -e "$lease" ] || fail leasefail_clears "lease file kept"

# bound_without_router_or_dns
reset; export ip=10.0.0.5 mask=8
"$hook" bound
not_logged bound_without_router_or_dns "route"
not_logged bound_without_router_or_dns "resolvectl dns"
expected=$(printf 'address=10.0.0.5/8\nrouter=\ndns=\n')
[ "$(cat "$lease")" = "$expected" ] || fail bound_without_router_or_dns "lease file: $(cat "$lease")"

# rejects_bad_interface
reset; full_lease; export interface='wlan0;rm'
if "$hook" bound 2>/dev/null; then fail rejects_bad_interface "accepted"; fi
[ ! -s "$CT_TEST_LOG" ] || fail rejects_bad_interface "ran commands"

echo "prelude udhcpc hook checks passed"
