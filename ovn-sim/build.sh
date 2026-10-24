#!/usr/bin/env bash
# Builds and pushes DPF's OVN-Kubernetes chart and dpf-utils image for simulated DPUs, from an
# ovn-kubernetes-dpf checkout with the simulateDPU toggle (dpuManifests.simulateDPU): DPF's released
# chart with that checkout's DPU templates, defaulting to the released dpf-utils image with that
# checkout's cniprovisioner. openshift-dpf deploys it with SIMULATED_DPUS_ONLY=true (OVN_CHART_URL,
# OVN_CHART_VERSION).
set -euo pipefail

SRC=${OVNK_DPF_SRC:?an ovn-kubernetes-dpf checkout with the simulateDPU toggle}
VERSION=${OVN_VERSION:-v26.4.1-ocp-release-v4.22}
IMAGE_REPO=${OVN_SIM_IMAGE_REPO:-quay.io/otuchfel/ovn-kubernetes-dpf-utils}
CHART_REPO=${OVN_SIM_CHART_REPO:-oci://quay.io/otuchfel/charts}
tag=${VERSION}-simdpu

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "dpf-utils image ${IMAGE_REPO}:${tag}"
for arch in amd64 arm64; do
    (cd "$SRC/dpf-utils" && CGO_ENABLED=0 GOOS=linux GOARCH=$arch \
        go build -trimpath -o "$work/cniprovisioner-$arch" ./cmd/dpucniprovisioner)
done
cat > "$work/Containerfile" <<EOF
FROM ghcr.io/mellanox/ovn-kubernetes-dpf-utils:${VERSION}
ARG TARGETARCH
COPY cniprovisioner-\${TARGETARCH} /cniprovisioner
EOF
podman manifest rm "${IMAGE_REPO}:${tag}" >/dev/null 2>&1 || true
podman build -q --platform linux/amd64,linux/arm64 --manifest "${IMAGE_REPO}:${tag}" "$work" >/dev/null
podman manifest push -q --all "${IMAGE_REPO}:${tag}" "docker://${IMAGE_REPO}:${tag}"

echo "chart ${CHART_REPO}/ovn-kubernetes-chart:${tag}"
helm pull -q oci://ghcr.io/mellanox/charts/ovn-kubernetes-chart --version "$VERSION" --untar --untardir "$work" 2>/dev/null \
    || helm pull oci://ghcr.io/mellanox/charts/ovn-kubernetes-chart --version "$VERSION" --untar --untardir "$work" >/dev/null
chart=$work/ovn-kubernetes-chart
cp "$SRC/helm/ovn-kubernetes-dpf/templates/dpu-manifests.yaml" "$chart/templates/"
IMAGE_REPO=$IMAGE_REPO TAG=$tag yq -i '
    .dpuManifests.imagedpf.repository = strenv(IMAGE_REPO) |
    .dpuManifests.imagedpf.tag = strenv(TAG) |
    .dpuManifests.simulateDPU = false' "$chart/values.yaml"
helm package "$chart" --version "$tag" --app-version "$tag" -d "$work" >/dev/null
helm push "$work/ovn-kubernetes-chart-${tag}.tgz" "$CHART_REPO" 2>&1 | grep -v '^Pushed\|^Digest' || true
echo "OVN_CHART_URL=${CHART_REPO} OVN_CHART_VERSION=${tag}"
