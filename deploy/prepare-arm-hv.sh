#!/usr/bin/env bash
# Prepares an aarch64 hypervisor for dpusimaas and its DPU VMs: packages, modular libvirt,
# mgmt-br, and storage. Idempotent. The host must already be RHSM-registered.
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

default_config="${XDG_CONFIG_HOME:-$HOME/.config}/dpusimaas/machines.env"
if [ -n "$config" ]; then
    # shellcheck disable=SC1090
    . "$config"
elif [ -f "$default_config" ]; then
    # shellcheck disable=SC1090,SC1091
    . "$default_config"
fi

HOST=${HOST:?the aarch64 hypervisor (ssh target)}
ARM_LAN_INTERFACE=${ARM_LAN_INTERFACE:-}
ARM_STORAGE_DIR=${ARM_STORAGE_DIR:-/var/lib/libvirt/images}
ARM_BRIDGE_MTU=${ARM_BRIDGE_MTU:-1500}

fail() {
    echo "$*" >&2
    exit 1
}

printf -v remote_env '%q ' \
    ARM_LAN_INTERFACE="$ARM_LAN_INTERFACE" \
    ARM_STORAGE_DIR="$ARM_STORAGE_DIR" \
    ARM_BRIDGE_MTU="$ARM_BRIDGE_MTU"

echo "Preparing $HOST for aarch64 simulator DPUs"
set +e
# shellcheck disable=SC2029
ssh "$HOST" "$remote_env bash -s" <<'REMOTE'
set -euo pipefail

fail() {
    echo "$*" >&2
    exit 1
}

[ "$(uname -m)" = aarch64 ] || fail "$HOSTNAME is $(uname -m), not aarch64"
if ! subscription-manager status >/dev/null; then
    fail "$HOSTNAME is not RHSM-registered; register it first"
fi

dnf -q -y --enablerepo=codeready-builder-for-rhel-10-aarch64-rpms \
    install qemu-kvm libvirt libvirt-client virt-install podman ipmitool openssl \
    policycoreutils-python-utils python3 python3-devel gcc libvirt-devel
systemctl enable --now virtqemud virtstoraged virtnetworkd virtlogd
if command -v firewall-cmd >/dev/null && systemctl is-active --quiet firewalld; then
    firewall-cmd --permanent --add-service=http >/dev/null
    firewall-cmd --reload >/dev/null
fi

install -d -m 0711 "$ARM_STORAGE_DIR"
if semanage fcontext -l | awk -v spec="$ARM_STORAGE_DIR(/.*)?" '$1 == spec { found = 1 } END { exit !found }'; then
    semanage fcontext -m -t virt_image_t "$ARM_STORAGE_DIR(/.*)?"
else
    semanage fcontext -a -t virt_image_t "$ARM_STORAGE_DIR(/.*)?"
fi
restorecon -R "$ARM_STORAGE_DIR"

if ! virsh pool-info default >/dev/null 2>&1; then
    virsh pool-define-as default dir --target "$ARM_STORAGE_DIR"
else
    current_target=$(virsh pool-dumpxml default | sed -n 's:.*<path>\(.*\)</path>.*:\1:p')
    [ "$current_target" = "$ARM_STORAGE_DIR" ] ||
        fail "libvirt pool default already uses $current_target; move it or change ARM_STORAGE_DIR"
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

if nmcli -g GENERAL.CONNECTION device show mgmt-br 2>/dev/null | grep -q .; then
    echo "mgmt-br is already configured"
    exit 0
fi

if [ -z "$ARM_LAN_INTERFACE" ]; then
    ARM_LAN_INTERFACE=$(ip route show default | awk '{for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit }}')
fi
[ -n "$ARM_LAN_INTERFACE" ] || fail "could not find the default-route interface; set ARM_LAN_INTERFACE"
[ "$(nmcli -g GENERAL.TYPE device show "$ARM_LAN_INTERFACE")" = ethernet ] ||
    fail "$ARM_LAN_INTERFACE is not an ethernet device"
old_connection=$(nmcli -g GENERAL.CONNECTION device show "$ARM_LAN_INTERFACE")
[ -n "$old_connection" ] || fail "$ARM_LAN_INTERFACE has no active NetworkManager connection"

bridge_settings=()
for family in ipv4 ipv6; do
    for property in method addresses gateway dns dns-search routes route-metric ignore-auto-dns never-default; do
        value=$(nmcli -g "$family.$property" connection show "$old_connection")
        [ -n "$value" ] || continue
        bridge_settings+=("$family.$property" "$value")
    done
done

nmcli connection add type bridge ifname mgmt-br con-name bridge-mgmt-br \
    bridge.stp no 802-3-ethernet.mtu "$ARM_BRIDGE_MTU" "${bridge_settings[@]}"
nmcli connection add type ethernet ifname "$ARM_LAN_INTERFACE" \
    con-name "bridge-slave-$ARM_LAN_INTERFACE-mgmt-br" master mgmt-br \
    802-3-ethernet.mtu "$ARM_BRIDGE_MTU"

log=/var/log/dpusimaas-mgmt-br.log
OLD_CONNECTION="$old_connection" nohup bash -c '
    set -euo pipefail
    nmcli connection up bridge-mgmt-br
    nmcli connection delete "$OLD_CONNECTION"
' </dev/null >"$log" 2>&1 &
echo "Converting $ARM_LAN_INTERFACE to mgmt-br; the SSH connection may drop (log: $log)"
sleep 10
REMOTE
status=$?
set -e

if [ "$status" -ne 0 ]; then
    if [ "$status" -eq 255 ]; then
        echo "The SSH connection ended while mgmt-br activation may still be in progress" >&2
    else
        echo "Preparing $HOST failed with status $status" >&2
        exit "$status"
    fi
fi

for _ in $(seq 60); do
    # shellcheck disable=SC2029
    bridge_state=$(ssh -o BatchMode=yes -o ConnectTimeout=5 "$HOST" \
        'nmcli -g GENERAL.STATE device show mgmt-br' 2>/dev/null || true)
    case "$bridge_state" in
        connected|'100 (connected)') break ;;
    esac
    sleep 2
done
case "$bridge_state" in
    connected|'100 (connected)') ;;
    *) fail "mgmt-br did not become connected on $HOST" ;;
esac
