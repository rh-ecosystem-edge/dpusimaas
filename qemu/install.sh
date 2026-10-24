#!/usr/bin/env bash
# Builds the host's own qemu-kvm with the igb patches next to it and installs it on $HOST (an x86
# RHEL libvirt host) as /usr/libexec/qemu-kvm-bf3sim. The stock qemu-kvm is untouched; a host VM
# uses the patched one by naming it as its <emulator>.
#
# The patches give QEMU's igb NIC what a BlueField's host side has:
#   igb-vpd.patch             a VPD with the card's serial number (x-vpd-serial), and ARI so the
#                             two ports read as one dual-port card (x-pcie-ari-nextfn-1)
#   igb-no-vf-loopback.patch  no VF-to-VF switching inside the NIC (x-vf-loopback=false): all VF
#                             traffic goes out to the DPU, as through a BlueField's eswitch
#
# The build runs in a UBI container of the host's RHEL release, with the host's subscription. It
# takes a while, and is skipped when the installed binary was built from the same qemu-kvm and
# patches.
set -euo pipefail

HOST=${HOST:?the x86 hypervisor (ssh target)}
here="$(cd "$(dirname "$0")" && pwd)"
patches=(igb-vpd.patch igb-no-vf-loopback.patch)
work=/var/lib/dpusimaas-qemu
target=/usr/libexec/qemu-kvm-bf3sim

stamp=$(cd "$here" && cat "${patches[@]}" | sha256sum | cut -c1-16)
ssh "$HOST" "mkdir -p $work"
for p in "${patches[@]}"; do scp -q "$here/$p" "$HOST:$work/$p"; done

ssh "$HOST" "WORK=$work TARGET=$target STAMP=$stamp bash -s" <<'EOF'
set -euo pipefail
nvr=$(rpm -q --qf '%{VERSION}-%{RELEASE}' qemu-kvm)
# The dist tag (.el9_6) must match the installed qemu-kvm's: its modules only load into the same build.
dist=$(rpm -q --qf '%{RELEASE}' qemu-kvm | grep -o '\.el[0-9_]*')
ubi="registry.access.redhat.com/ubi$(rpm -E %rhel)/ubi:$(sed 's/.*release \([0-9.]*\).*/\1/' /etc/redhat-release)"
want="qemu-kvm-$nvr $STAMP"
if [ -x "$TARGET" ] && [ "$(cat "$TARGET.built-from" 2>/dev/null)" = "$want" ]; then
    echo "$TARGET is already built from $want"
    exit 0
fi
echo "Building qemu-kvm-$nvr with the igb patches in $ubi (this takes a while)"
# Bounded parallelism and no process limit: with all of a big host's cores, the build runs out of
# processes and threads.
podman run --rm --pids-limit=-1 -v "$WORK:/work:Z" "$ubi" bash -euc "
    dnf -q -y install rpm-build dnf-plugins-core >/dev/null
    cd /work
    [ -f qemu-kvm-$nvr.src.rpm ] || dnf -q download --source --enablerepo=rhel-\$(rpm -E %rhel)-for-x86_64-appstream-source-rpms qemu-kvm-$nvr
    rpm -i qemu-kvm-$nvr.src.rpm
    dnf -q -y builddep --enablerepo=codeready-builder-for-rhel-\$(rpm -E %rhel)-x86_64-rpms ~/rpmbuild/SPECS/qemu-kvm.spec >/dev/null
    cp igb-vpd.patch igb-no-vf-loopback.patch ~/rpmbuild/SOURCES/
    last=\$(grep -n '^Patch[0-9]*:' ~/rpmbuild/SPECS/qemu-kvm.spec | tail -1 | cut -d: -f1)
    sed -i \"\${last}a Patch9999: igb-vpd.patch\nPatch10000: igb-no-vf-loopback.patch\" ~/rpmbuild/SPECS/qemu-kvm.spec
    rpmbuild -bc --define 'dist $dist' --define '_smp_mflags -j32' ~/rpmbuild/SPECS/qemu-kvm.spec > /work/build.log 2>&1 || { tail -40 /work/build.log; exit 1; }
    cp ~/rpmbuild/BUILD/qemu-*/qemu_kvm_build/qemu-system-x86_64 /work/qemu-kvm-bf3sim
"
install -m 0755 "$WORK/qemu-kvm-bf3sim" "$TARGET"
chcon -t qemu_exec_t "$TARGET"
echo "$want" > "$TARGET.built-from"
"$TARGET" -device igb,help | grep -E 'x-vpd-serial|x-vf-loopback' >/dev/null
echo "Installed $TARGET (qemu-kvm-$nvr with the igb patches)"
EOF
