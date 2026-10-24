#!/usr/bin/env bash
# Lets dpusimaas on the aarch64 hypervisor ($ARM_HV) manage VMs on the x86 one ($X86_HV) over
# libvirt+SSH (qemu+ssh://root@$X86_HV_ADDR/system): authorizes the ARM hypervisor's root key on
# the x86 one and records the x86 one's host key. Idempotent.
set -euo pipefail

ARM_HV=${ARM_HV:?the aarch64 hypervisor (ssh target)}
X86_HV=${X86_HV:?the x86 hypervisor (ssh target)}
X86_HV_ADDR=${X86_HV_ADDR:?the address dpusimaas reaches the x86 hypervisor at}

key=$(ssh "$ARM_HV" 'test -f /root/.ssh/id_ed25519.pub || ssh-keygen -q -t ed25519 -N "" -f /root/.ssh/id_ed25519; cat /root/.ssh/id_ed25519.pub')
printf '%s\n' "$key" | ssh "$X86_HV" 'mkdir -p -m 700 /root/.ssh; k=$(cat); grep -qxF "$k" /root/.ssh/authorized_keys 2>/dev/null || printf "%s dpusimaas\n" "$k" >> /root/.ssh/authorized_keys'
ssh "$ARM_HV" "ssh-keygen -F $X86_HV_ADDR >/dev/null || ssh-keyscan -q $X86_HV_ADDR >> /root/.ssh/known_hosts"
ssh "$ARM_HV" "ssh -o BatchMode=yes root@$X86_HV_ADDR virsh -c qemu:///system version --daemon | head -1"
echo "dpusimaas on $ARM_HV can manage libvirt on $X86_HV ($X86_HV_ADDR)"
