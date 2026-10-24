#!/usr/bin/env bash
# Smoke test of a deployed dpusimaas: leases a Zero Trust DPU, checks its BMCs the way DPF and the
# automation use them (Redfish serial and rshim, IPMI power), and ends the lease. With $X86_HV, does
# the same for a host-trusted DPU host (its host's Redfish and IPMI, its host VM and wire). ipmitool
# and the leftover checks run on $HOST (and $X86_HV).
set -euo pipefail

URL=${URL:-http://dpusimaas.tuchfeld.dev}
HOST=${HOST:-aarchv}
X86_HV=${X86_HV:-}
OWNER=${OWNER:-smoke-test}
PASSWORD=SmokeTest123

fail() { echo "FAIL: $*" >&2; [ -n "${id:-}" ] && curl -s -X DELETE "$URL/v1/dpus/$id" >/dev/null; exit 1; }
field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }

lease=$(curl -sf --max-time 120 -X POST "$URL/v1/dpus" -d "{\"owner\":\"$OWNER\",\"mode\":\"zero-trust\",\"bmc_password\":\"$PASSWORD\",\"ttl_hours\":1}") \
    || fail "couldn't lease a DPU"
id=$(field "['id']" <<<"$lease"); ip=$(field "['ip']" <<<"$lease"); serial=$(field "['serial']" <<<"$lease")
ipmi_user=$(field "['ipmi_user']" <<<"$lease"); ipmi_password=$(field "['ipmi_password']" <<<"$lease")
echo "leased $id at $ip (serial $serial)"

got=$(curl -sfk --max-time 30 -u "root:$PASSWORD" "https://$ip/redfish/v1/Systems/Bluefield" | field "['SerialNumber']") \
    || fail "Redfish doesn't answer at $ip"
[ "$got" = "$serial" ] || fail "Redfish serial $got, want $serial"
echo "ok   Redfish answers with serial $got"

rshim=$(curl -sfk --max-time 30 -u "root:$PASSWORD" "https://$ip/redfish/v1/Managers/Bluefield_BMC/Oem/Nvidia" | field "['BmcRShim']['BmcRShimEnabled']")
[ "$rshim" = True ] || fail "rshim is $rshim"
echo "ok   rshim enabled"

ipmi() { ssh "$HOST" "IPMI_PASSWORD='$ipmi_password' ipmitool -I lanplus -H $ip -U $ipmi_user -E chassis power $1"; }
[ "$(ipmi status)" = "Chassis Power is on" ] || fail "IPMI power status: $(ipmi status)"
ipmi cycle >/dev/null || fail "IPMI power cycle"
curl -sf "$URL/v1/dpus/$id" | grep -q '"host cycle"' || fail "the power cycle didn't reach dpusimaas"
echo "ok   IPMI power status and cycle"

curl -sf -o /dev/null -X DELETE "$URL/v1/dpus/$id" || fail "couldn't end the lease"
left=$(ssh "$HOST" "{ ip netns list; ip -br link; } | grep -cE 'dpusim-$id|dsa-$id' || true")
[ "$left" = 0 ] || fail "lease $id left its namespace or veth behind"
id=
echo "ok   lease ended, nothing left behind"

if [ -n "$X86_HV" ]; then
    lease=$(curl -sf --max-time 300 -X POST "$URL/v1/dpus" -d "{\"owner\":\"$OWNER\",\"mode\":\"host-trusted\",\"ttl_hours\":1}") \
        || fail "couldn't lease a host-trusted DPU host"
    id=$(field "['id']" <<<"$lease"); ip=$(field "['ip']" <<<"$lease"); vm=$(field "['host']['vm']" <<<"$lease")
    ipmi_user=$(field "['ipmi_user']" <<<"$lease"); ipmi_password=$(field "['ipmi_password']" <<<"$lease")
    echo "leased $id at $ip (host VM $vm)"

    systems=$(curl -sfk --max-time 30 -u "$ipmi_user:$ipmi_password" "https://$ip/redfish/v1/Systems" | field "['Members@odata.count']") \
        || fail "the host's Redfish doesn't answer at $ip"
    [ "$systems" = 1 ] || fail "the host's Redfish shows $systems systems, want 1"
    [ "$(curl -sk -o /dev/null -w '%{http_code}' "https://$ip/redfish/v1/Systems")" = 401 ] || fail "the host's Redfish answers without a password"
    echo "ok   the host's Redfish shows its one system, with a password"

    [ "$(ipmi status)" = "Chassis Power is off" ] || fail "IPMI power status: $(ipmi status)"
    ssh "$X86_HV" "virsh dominfo $vm >/dev/null && ip link show dsw-$id >/dev/null" || fail "no host VM or wire on $X86_HV"
    echo "ok   host VM and wire on $X86_HV, powered off"

    curl -sf -o /dev/null -X DELETE "$URL/v1/dpus/$id" || fail "couldn't end the lease"
    left=$(ssh "$HOST" "{ ip netns list; ip -br link; } | grep -c $id || true")
    left_x86=$(ssh "$X86_HV" "{ virsh list --all --name; virsh vol-list default; ip -br link; } | grep -c $id || true")
    [ "$left" = 0 ] && [ "$left_x86" = 0 ] || fail "lease $id left things behind ($left here, $left_x86 on $X86_HV)"
    id=
    echo "ok   lease ended, nothing left behind"
fi
echo PASS
