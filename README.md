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

## Running your own

`make deploy ARM_HV=<ssh host> X86_HV=<ssh host>` installs it on an aarch64 libvirt host (DPUs) and
an x86 one (hosts; `make qemu` first). `make smoke` checks it.

dpusimaas itself is Rust (`src/`). Each DPU's BMC is `mockbmc` (`mockbmc/`), a small Go wrapper
around upstream DPF's mock BlueField Redfish server, so it stays the same as upstream; dpusimaas runs
one per lease. The host's IPMI BMC is `src/ipmi_bmc.py` (pyghmi), its Redfish sushy-tools
(`src/redfish_bmc.py`). `make hostdriver-image` and `make ovn-sim` build the images and chart
`SIMULATED_DPUS_ONLY` deploys.
