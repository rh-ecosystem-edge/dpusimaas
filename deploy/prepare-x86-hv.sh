#!/usr/bin/env bash
# Prepares an x86 hypervisor for dpusimaas host VMs: packages, libvirt, br0, storage, and the
# patched QEMU. Idempotent. The host must already be RHSM-registered; credentials are deliberately
# not handled here.
set -euo pipefail

config=${CONFIG:-}
while [ $# -gt 0 ]; do
    case "$1" in
        --config)
            [ $# -ge 2 ] || { echo "--config needs a file" >&2; exit 2; }
            config=$2
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

here="$(cd "$(dirname "$0")" && pwd)"
default_config="${XDG_CONFIG_HOME:-$HOME/.config}/dpusimaas/machines.env"
if [ -n "$config" ]; then
    # shellcheck disable=SC1090
    . "$config"
elif [ -f "$default_config" ]; then
    # shellcheck disable=SC1090,SC1091
    . "$default_config"
fi

X86_HV=${X86_HV:?the x86 hypervisor (ssh target)}
X86_LAN_INTERFACE=${X86_LAN_INTERFACE:-}
X86_STORAGE_DIR=${X86_STORAGE_DIR:-/var/lib/libvirt/images}
X86_BRIDGE_MTU=${X86_BRIDGE_MTU:-9000}
BUILD_QEMU=${BUILD_QEMU:-1}

case "$BUILD_QEMU" in
    0|1) ;;
    *)
        echo "BUILD_QEMU must be 0 or 1" >&2
        exit 2
        ;;
esac

fail() {
    echo "$*" >&2
    exit 1
}

printf -v remote_env '%q ' \
    X86_LAN_INTERFACE="$X86_LAN_INTERFACE" \
    X86_STORAGE_DIR="$X86_STORAGE_DIR" \
    X86_BRIDGE_MTU="$X86_BRIDGE_MTU"

echo "Preparing $X86_HV for x86 simulator hosts"
set +e
# shellcheck disable=SC2029
ssh "$X86_HV" "$remote_env bash -s" <<'REMOTE'
set -euo pipefail

fail() {
    echo "$*" >&2
    exit 1
}

if ! subscription-manager status >/dev/null; then
    fail "$HOSTNAME is not RHSM-registered. Register it (or set up an activation key) first; this script does not handle credentials."
fi

dnf -q -y install qemu-kvm libvirt libvirt-client virt-install podman policycoreutils-python-utils
systemctl enable --now libvirtd

install -d -m 0711 "$X86_STORAGE_DIR"
if semanage fcontext -l | awk -v spec="$X86_STORAGE_DIR(/.*)?" '$1 == spec { found = 1 } END { exit !found }'; then
    semanage fcontext -m -t virt_image_t "$X86_STORAGE_DIR(/.*)?"
else
    semanage fcontext -a -t virt_image_t "$X86_STORAGE_DIR(/.*)?"
fi
restorecon -R "$X86_STORAGE_DIR"

if ! virsh pool-info default >/dev/null 2>&1; then
    virsh pool-define-as default dir --target "$X86_STORAGE_DIR"
else
    current_target=$(virsh pool-dumpxml default | sed -n 's:.*<path>\(.*\)</path>.*:\1:p')
    [ "$current_target" = "$X86_STORAGE_DIR" ] ||
        fail "libvirt pool default already uses $current_target; move it or change X86_STORAGE_DIR"
fi
for _ in $(seq 30); do
    virsh pool-start default >/dev/null 2>&1 || true
    pool_state=$(virsh pool-info default)
    [[ "$pool_state" =~ State:[[:space:]]+(running|active) ]] && break
    sleep 1
done
pool_state=$(virsh pool-info default)
[[ "$pool_state" =~ State:[[:space:]]+(running|active) ]] || fail "libvirt pool default did not start"
virsh pool-autostart default >/dev/null

if nmcli -g GENERAL.CONNECTION device show br0 2>/dev/null | grep -q .; then
    echo "br0 is already configured"
    exit 0
fi

if [ -z "$X86_LAN_INTERFACE" ]; then
    X86_LAN_INTERFACE=$(ip route show default | awk '{for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit }}')
fi
[ -n "$X86_LAN_INTERFACE" ] || fail "could not find the default-route interface; set X86_LAN_INTERFACE"
[ "$(nmcli -g GENERAL.TYPE device show "$X86_LAN_INTERFACE")" = ethernet ] ||
    fail "$X86_LAN_INTERFACE is not an ethernet device"
old_connection=$(nmcli -g GENERAL.CONNECTION device show "$X86_LAN_INTERFACE")
[ -n "$old_connection" ] || fail "$X86_LAN_INTERFACE has no active NetworkManager connection"

bridge_settings=()
for family in ipv4 ipv6; do
    for property in method addresses gateway dns dns-search routes route-metric ignore-auto-dns never-default; do
        value=$(nmcli -g "$family.$property" connection show "$old_connection")
        [ -n "$value" ] || continue
        bridge_settings+=("$family.$property" "$value")
    done
done

nmcli connection add type bridge ifname br0 con-name bridge-br0 \
    bridge.stp no 802-3-ethernet.mtu "$X86_BRIDGE_MTU" "${bridge_settings[@]}"
nmcli connection add type ethernet ifname "$X86_LAN_INTERFACE" \
    con-name "bridge-slave-$X86_LAN_INTERFACE" master br0 \
    802-3-ethernet.mtu "$X86_BRIDGE_MTU"

# Activation can replace the route that carries this SSH session. Keep it alive beyond the session
# and leave a log behind, then give it time to start before the caller polls for reconnection.
log=/var/log/dpusimaas-br0.log
OLD_CONNECTION="$old_connection" nohup bash -c '
    set -euo pipefail
    nmcli connection up bridge-br0
    nmcli connection delete "$OLD_CONNECTION"
' </dev/null >"$log" 2>&1 &
echo "Converting $X86_LAN_INTERFACE to br0; the SSH connection may drop (log: $log)"
sleep 10
REMOTE
status=$?
set -e

if [ "$status" -ne 0 ]; then
    if [ "$status" -eq 255 ]; then
        echo "The SSH connection ended while br0 activation may still be in progress" >&2
    else
        echo "Preparing $X86_HV failed with status $status" >&2
        exit "$status"
    fi
fi

for _ in $(seq 60); do
    # shellcheck disable=SC2029
    bridge_state=$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$X86_HV" \
        'nmcli -g GENERAL.STATE device show br0' 2>/dev/null || true)
    case "$bridge_state" in
        connected|'100 (connected)') break ;;
    esac
    sleep 2
done
case "$bridge_state" in
    connected|'100 (connected)') ;;
    *) fail "br0 did not become connected on $X86_HV" ;;
esac

if [ "$BUILD_QEMU" = 1 ]; then
    HOST="$X86_HV" "$here/../qemu/install.sh"
else
    echo "Skipping patched QEMU (BUILD_QEMU=0)"
fi
