#!/usr/bin/env bash
# Installs bin/dpusimaas, bin/mockbmc and bin/dpusim (built by `make build`) on $HOST under $PREFIX, with the
# python venv the BMCs need, and (re)starts the service. With $X86_HV (host-trusted leases' hosts),
# also lets dpusimaas drive it and installs bin/dpusim-x86_64 there. Idempotent.
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

HOST=${HOST:?the aarch64 hypervisor (ssh target)}
PREFIX=${PREFIX:-/opt/dpusimaas}
X86_HV=${X86_HV:-}
MAX_DPUS=${MAX_DPUS:-4}
MAX_HOSTS=${MAX_HOSTS:-3}
bin="$here/../bin"

for f in dpusimaas mockbmc dpusim ${X86_HV:+dpusim-x86_64}; do
    [ -x "$bin/$f" ] || { echo "missing $bin/$f: run make build" >&2; exit 1; }
done

echo "Preparing $HOST:$PREFIX"
# shellcheck disable=SC2029
ssh "$HOST" "PREFIX=$PREFIX bash -s" <<'EOF'
set -euo pipefail
mkdir -p "$PREFIX"
[ -x "$PREFIX/venv/bin/python" ] || python3 -m venv "$PREFIX/venv"
pip() { "$PREFIX/venv/bin/pip" install -q --disable-pip-version-check "$@"; }
# libvirt-python builds against libvirt's headers. Installing them can upgrade libvirt, which this
# machine's other VMs run on: that's left to whoever runs it.
pip pyghmi sushy-tools bcrypt libvirt-python || {
    echo "building libvirt-python failed: it needs libvirt-devel, python3-devel and gcc on $(hostname)" >&2
    exit 1
}
"$PREFIX/venv/bin/python" -c 'import pyghmi.ipmi.bmc, sushy_tools.emulator.main, bcrypt, libvirt'
EOF

x86_args=
if [ -n "$X86_HV" ]; then
    x86_addr=$(ssh -G "$X86_HV" | awk '/^hostname /{print $2}')
    echo "Letting dpusimaas drive $X86_HV ($x86_addr)"
    ARM_HV=$HOST X86_HV=$X86_HV X86_HV_ADDR=$x86_addr PREFIX=$PREFIX "$here/trust-x86-hv.sh"
    # shellcheck disable=SC2029
    ssh "$X86_HV" "mkdir -p $PREFIX"
    scp -q "$bin/dpusim-x86_64" "$X86_HV:$PREFIX/dpusim.new"
    # shellcheck disable=SC2029
    ssh "$X86_HV" "mv -f $PREFIX/dpusim.new $PREFIX/dpusim"
    x86_args="--x86-hypervisor root@$x86_addr --x86-dpusim $PREFIX/dpusim"
fi

echo "Copying binaries and the unit"
# Copy next to the running binaries and swap them in, so a running service isn't hit mid-copy.
scp -q "$bin/dpusimaas" "$HOST:$PREFIX/dpusimaas.new"
scp -q "$bin/mockbmc" "$HOST:$PREFIX/mockbmc.new"
scp -q "$bin/dpusim" "$HOST:$PREFIX/dpusim.new"
sed \
    -e "s|@PREFIX@|$PREFIX|g" \
    -e "s|@X86_ARGS@|$x86_args|" \
    -e "s|@MAX_DPUS@|$MAX_DPUS|g" \
    -e "s|@MAX_HOSTS@|$MAX_HOSTS|g" \
    "$here/dpusimaas.service" | ssh "$HOST" "cat > /etc/systemd/system/dpusimaas.service"

# shellcheck disable=SC2029
ssh "$HOST" "PREFIX=$PREFIX bash -s" <<'EOF'
set -euo pipefail
mv -f "$PREFIX/dpusimaas.new" "$PREFIX/dpusimaas"
mv -f "$PREFIX/mockbmc.new" "$PREFIX/mockbmc"
mv -f "$PREFIX/dpusim.new" "$PREFIX/dpusim"
systemctl daemon-reload
systemctl enable -q dpusimaas
systemctl restart dpusimaas
# Restoring the leases (DHCP, host VMs) comes first.
for _ in $(seq 120); do curl -sf localhost/healthz >/dev/null && break; sleep 1; done
systemctl is-active dpusimaas
curl -s localhost/v1/info
EOF
