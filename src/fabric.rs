//! The leaf router a cluster's simulated DPUs are cabled to (`dpusim leaf`): one per cluster, on
//! this hypervisor, started with the cluster's first DPU and removed with its last. Each DPU's
//! uplink goes on the bridge of its own leaf port; the cluster's nodes route the DPUs' VTEP
//! subnet through the leaf's LAN address (GET /v1/fabric).

use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    command::{output, run},
    lease::Lease,
};

/// The bridge the lease's DPU uplink goes on.
pub fn uplink_bridge(lease: &Lease) -> String {
    format!("dsu-{}", lease.id)
}

pub async fn attach(dpusim: &Path, lease: &Lease, lan_bridge: &str) -> Result<()> {
    run(
        dpusim,
        [
            "leaf",
            "attach",
            &lease.cluster,
            &lease.id,
            "--lan-bridge",
            lan_bridge,
        ],
    )
    .await
    .with_context(|| format!("attaching the DPU to the leaf of {}", lease.cluster))
}

pub async fn detach(dpusim: &Path, lease: &Lease) -> Result<()> {
    run(dpusim, ["leaf", "detach", &lease.cluster, &lease.id]).await
}

/// The leaf's LAN address, if the cluster has one.
pub async fn address(dpusim: &Path, cluster: &str) -> Option<String> {
    output(dpusim, ["leaf", "address", cluster])
        .await
        .ok()
        .map(|address| address.trim().to_owned())
}
