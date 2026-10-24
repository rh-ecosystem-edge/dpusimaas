//! Each Zero Trust lease lives in its own network namespace, plugged into the LAN bridge with a
//! veth whose namespace end carries the lease's MAC. Its BMCs listen there, on the address DHCP
//! gave that MAC.

use std::{fs::File, path::Path, thread};

use anyhow::{Context, Result};
use nix::sched::{CloneFlags, setns};
use tokio::sync::oneshot;

use crate::{command::run, lease::Lease};

pub fn namespace_name(lease_id: &str) -> String {
    format!("dpusim-{lease_id}")
}

fn veth_name(lease_id: &str) -> String {
    format!("dsa-{lease_id}")
}

/// Creates the lease's namespace and plugs it into the bridge (idempotent).
pub async fn ensure(lease: &Lease, bridge: &str) -> Result<()> {
    let namespace = namespace_name(&lease.id);
    let veth = veth_name(&lease.id);
    if !Path::new("/run/netns").join(&namespace).exists() {
        run("ip", ["netns", "add", &namespace]).await?;
    }
    if !Path::new("/sys/class/net").join(&veth).exists() {
        run(
            "ip",
            [
                "link", "add", &veth, "type", "veth", "peer", "name", "eth0", "netns", &namespace,
            ],
        )
        .await?;
    }
    let link_setup: [&[&str]; 4] = [
        &["link", "set", &veth, "master", bridge, "up"],
        &["-n", &namespace, "link", "set", "lo", "up"],
        &[
            "-n", &namespace, "link", "set", "eth0", "address", &lease.mac,
        ],
        &["-n", &namespace, "link", "set", "eth0", "up"],
    ];
    for args in link_setup {
        run("ip", args).await?;
    }
    run_in(&lease.id, disable_reverse_path_filter)
        .await
        .context("turning off reverse path filtering")
}

/// Until DHCP gives it an address, the namespace has no route to anything, so reverse path
/// filtering (loose by default on Fedora and RHEL) drops the DHCP server's answers. The namespace
/// has one interface: the filter protects nothing there. Must run inside the namespace.
fn disable_reverse_path_filter() -> Result<()> {
    ["all", "eth0"].into_iter().try_for_each(|interface| {
        let setting = format!("/proc/sys/net/ipv4/conf/{interface}/rp_filter");
        std::fs::write(&setting, "0").with_context(|| format!("writing {setting}"))
    })
}

/// Removes the lease's veth and namespace, whichever exist.
pub async fn delete(lease_id: &str) {
    let veth = veth_name(lease_id);
    let namespace = namespace_name(lease_id);
    if Path::new("/sys/class/net").join(&veth).exists()
        && let Err(error) = run("ip", ["link", "del", &veth]).await
    {
        tracing::warn!(lease = lease_id, "{error:#}");
    }
    if Path::new("/run/netns").join(&namespace).exists()
        && let Err(error) = run("ip", ["netns", "del", &namespace]).await
    {
        tracing::warn!(lease = lease_id, "{error:#}");
    }
}

/// Runs `work` on a thread of its own inside the lease's namespace. Sockets it opens stay in that
/// namespace afterwards. The thread ends with `work`, so no other code runs in the namespace.
pub async fn run_in<T: Send + 'static>(
    lease_id: &str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let namespace_path = Path::new("/run/netns").join(namespace_name(lease_id));
    let namespace = File::open(&namespace_path)
        .with_context(|| format!("opening {}", namespace_path.display()))?;
    let (result_sender, result_receiver) = oneshot::channel();
    thread::Builder::new()
        .name(format!("netns-{lease_id}"))
        .spawn(move || {
            let result = setns(&namespace, CloneFlags::CLONE_NEWNET)
                .context("entering the namespace")
                .and_then(|()| work());
            // The receiver only goes away when the caller does.
            let _ = result_sender.send(result);
        })
        .context("starting a thread for the namespace")?;
    result_receiver
        .await
        .context("waiting for the namespace's thread")?
        .with_context(|| format!("in namespace {}", namespace_name(lease_id)))
}
