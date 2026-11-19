#!/usr/bin/env bash
# Lets dpusimaas on the aarch64 hypervisor ($ARM_HV) manage VMs on the x86 one ($X86_HV) over
# libvirt+SSH (qemu+ssh://root@$X86_HV_ADDR/system): authorizes a dedicated dpusimaas key on the
# x86 one, records the x86 one's host key in dpusimaas's own known-hosts file, and removes the
# root-key entries an older install added. Idempotent.
set -euo pipefail

ARM_HV=${ARM_HV:?the aarch64 hypervisor (ssh target)}
X86_HV=${X86_HV:?the x86 hypervisor (ssh target)}
X86_HV_ADDR=${X86_HV_ADDR:?the address dpusimaas reaches the x86 hypervisor at}
PREFIX=${PREFIX:?the dpusimaas install prefix}

printf -v remote_env '%q ' PREFIX="$PREFIX" X86_HV_ADDR="$X86_HV_ADDR"
key=$(ssh "$ARM_HV" "$remote_env bash -s" <<'REMOTE'
set -euo pipefail

ssh_dir=$PREFIX/.ssh
known_hosts=$ssh_dir/known_hosts
key=$ssh_dir/id_ed25519
mkdir -p -m 700 "$ssh_dir"
[ -f "$key" ] || ssh-keygen -q -t ed25519 -N "" -C "dpusimaas-$X86_HV_ADDR" -f "$key"
chmod 700 "$ssh_dir"
chmod 600 "$key"
[ ! -f "$known_hosts" ] || chmod 600 "$known_hosts"
chmod 644 "$key.pub"
if ! ssh-keygen -F "$X86_HV_ADDR" -f "$known_hosts" >/dev/null; then
    ssh-keyscan -q "$X86_HV_ADDR" >> "$known_hosts"
fi
ssh-keygen -F "$X86_HV_ADDR" -f "$known_hosts" >/dev/null
chmod 600 "$known_hosts"

mkdir -p -m 700 "$PREFIX/ssh"
wrapper=$PREFIX/ssh/ssh
ssh_bin=$(command -v ssh)
{
    echo '#!/bin/bash'
    printf 'exec %q -i %q -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=%q "$@"\n' \
        "$ssh_bin" "$key" "$known_hosts"
} > "$wrapper"
chmod 700 "$wrapper"
cat "$key.pub"
REMOTE
)
old_key=$(ssh "$ARM_HV" '[ ! -f /root/.ssh/id_ed25519.pub ] || cat /root/.ssh/id_ed25519.pub')

printf -v remote_env '%q ' key="$key" old_key="$old_key"
ssh "$X86_HV" "$remote_env bash -s" <<'REMOTE'
set -euo pipefail

mkdir -p -m 700 /root/.ssh
chmod 700 /root/.ssh
auth=/root/.ssh/authorized_keys
tmp=$(mktemp "${auth}.tmp.XXXXXX")
trap 'rm -f "$tmp"' EXIT
{
    [ ! -s "$auth" ] || awk -v key="$key" -v old_key="$old_key" '
        $0 == key " dpusimaas" { next }
        $0 == old_key " dpusimaas" { next }
        $0 == key { next }
        { print }
    ' "$auth"
    printf '%s\n' "$key"
} > "$tmp"
chmod 600 "$tmp"
mv "$tmp" "$auth"
trap - EXIT
REMOTE

printf -v wrapper '%q' "$PREFIX/ssh/ssh"
ssh "$ARM_HV" "$remote_env $wrapper root@$X86_HV_ADDR virsh -c qemu:///system version --daemon"
echo "dpusimaas on $ARM_HV can manage libvirt on $X86_HV ($X86_HV_ADDR)"
