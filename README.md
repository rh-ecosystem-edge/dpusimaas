# dpusimaas

Simulated BlueField DPUs on demand, for deploying and testing DPF without DPU hardware.

## What you get

A simulated DPU host. To DPF it looks like a lab machine.

Zero Trust:

- A DPU BMC that DPF provisions over Redfish, as with real hardware.
- A host BMC (IPMI) that DPF power-cycles the host through.
- Once DPF installs the DPU, a real DPU OS boots, runs DPF's DPU agent, and joins your DPU cluster.

Host-trusted:

- A host your cluster installs as a worker, through its BMC (Redfish with virtual media, IPMI).
- Its DPU: once DPF installs it through the host, a real DPU OS boots, runs DPF's DPU agent (which
  talks to the host's, as on a BlueField), and joins your DPU cluster.
- Deploy with openshift-dpf's `SIMULATED_DPUS_ONLY=true` (dpf-dev sets it for you).

It's yours until you release it.

## Using it

From dpf-dev: in the DPU hosts card, choose **Create a simulated DPU host**. It fills in the worker
for you.

Directly: see the API in [openapi.yaml](openapi.yaml).

## Limits

- A deployment's DPU hosts are all simulated or all real.
- Capacity is limited by the hypervisors' memory: each DPU and each host takes 16 GiB.
- DPU and host slots are independent. A zero-trust lease uses one DPU slot; a host-trusted lease
  uses one DPU slot and one host slot.

## Running your own

`make deploy` installs it on the aarch64 and x86 hypervisors named in your external machine
inventory. `make smoke` checks it.

Put host-specific settings in `~/.config/dpusimaas/machines.env` (see
`deploy/dpusimaas.env.example`) and run `make prepare-arm` and `make prepare-x86` to prepare the
hypervisors:
packages, libvirt, idempotent LAN bridges, storage, and (on x86) the patched QEMU. Each hypervisor
must already be RHSM-registered; the scripts intentionally do not handle registration credentials.
Then run `make deploy CONFIG=...` to build and install dpusimaas with the configured hypervisors and
slot limits.

dpusimaas itself is Rust (`src/`). Each DPU's BMC is `mockbmc` (`mockbmc/`), a small Go wrapper
around upstream DPF's mock BlueField Redfish server, so it stays the same as upstream; dpusimaas runs
one per lease. The host's IPMI BMC is `src/ipmi_bmc.py` (pyghmi), its Redfish sushy-tools
(`src/redfish_bmc.py`). `make hostdriver-image` and `make ovn-sim` build the images and chart
`SIMULATED_DPUS_ONLY` deploys.
