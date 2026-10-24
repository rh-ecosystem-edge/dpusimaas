# Build dpusimaas and the dpusim it drives, and deploy them.
#
#   make build                 bin/dpusimaas, bin/mockbmc and bin/dpusim (aarch64)
#   make test                  dpusimaas's and mockbmc's tests
#   make deploy                build, then install on the hypervisors and restart
#   make smoke                 lease a DPU, check its BMCs, end the lease
#   make qemu                  build and install the patched QEMU on the x86 hypervisor ($(X86_HV))
#   make hostdriver-image      build and push the sim hostdriver image for DPF $(DPF_VERSION)
#   make ovn-sim               build and push DPF's OVN chart and dpf-utils image for simulated DPUs
#   make status / make logs    the service on $(ARM_HV)
#
# dpusimaas, the BMCs and the DPU VMs run on the aarch64 hypervisor. Host-trusted leases' hosts run on
# the x86 hypervisor ($(X86_HV); X86_HV= for zero-trust only), driven from there over libvirt+SSH.
# dpusimaas is Rust; mockbmc (the DPU BMC, upstream DPF's mock Redfish server) is Go.
# dpusim comes from the openshift-dpf sim (sim/dpusim), at DPUSIM_SRC.

ARM_HV     ?= aarchv
X86_HV     ?= hv2
DPUSIM_SRC ?= $(HOME)/repos/openshift-dpf-sim/sim/dpusim
PREFIX     ?= /opt/dpusimaas
DPF_VERSION         ?= v26.4.1
HOSTDRIVER_SIM_REPO ?= quay.io/otuchfel/hostdriver-sim
OVNK_DPF_SRC        ?= $(HOME)/repos/ovn-kubernetes-dpf-sim

.PHONY: build build-dpusimaas build-mockbmc build-dpusim deploy smoke qemu hostdriver-image ovn-sim status logs test

build: build-dpusimaas build-mockbmc build-dpusim

build-dpusimaas:
	cargo fmt --check
	cargo clippy --release --target aarch64-unknown-linux-musl -- -D warnings
	cargo build --release --target aarch64-unknown-linux-musl
	mkdir -p bin
	cp target/aarch64-unknown-linux-musl/release/dpusimaas bin/dpusimaas

build-mockbmc:
	gofmt -l . | (! grep .) || (echo "gofmt the files above" >&2; exit 1)
	go vet ./...
	GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o bin/mockbmc ./mockbmc

build-dpusim:
	cd $(DPUSIM_SRC) && cargo build --release --target aarch64-unknown-linux-musl --target x86_64-unknown-linux-musl
	cp $(DPUSIM_SRC)/target/aarch64-unknown-linux-musl/release/dpusim bin/dpusim
	cp $(DPUSIM_SRC)/target/x86_64-unknown-linux-musl/release/dpusim bin/dpusim-x86_64

test:
	cargo test
	go test ./...

deploy: build
	HOST=$(ARM_HV) X86_HV=$(X86_HV) PREFIX=$(PREFIX) deploy/install.sh

qemu:
	HOST=$(X86_HV) qemu/install.sh

# Host-trusted hosts pull this in place of DPF's hostdriver image (same tag: a registry mirror).
hostdriver-image:
	podman build --build-arg BASE=nvcr.io/nvidia/doca/hostdriver:$(DPF_VERSION) -t $(HOSTDRIVER_SIM_REPO):$(DPF_VERSION) hostdriver-sim
	podman push $(HOSTDRIVER_SIM_REPO):$(DPF_VERSION)

# Simulated DPUs' OVN-Kubernetes: --simulate-dpu, and the host PF MAC from the node name.
ovn-sim:
	OVNK_DPF_SRC=$(OVNK_DPF_SRC) ovn-sim/build.sh

smoke:
	HOST=$(ARM_HV) X86_HV=$(X86_HV) deploy/smoke.sh

status:
	ssh $(ARM_HV) 'systemctl status --no-pager dpusimaas; curl -s localhost/v1/info'

logs:
	ssh $(ARM_HV) 'journalctl -u dpusimaas --no-pager -n 200'
