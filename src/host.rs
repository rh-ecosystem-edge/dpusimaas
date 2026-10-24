//! A host-trusted lease's host: an x86 VM on the x86 hypervisor (`dpusim host-vm`), driven from
//! here over libvirt+SSH. The cluster installs it like any bare-metal worker, through the lease's
//! host BMC (Redfish with virtual media, redfish_bmc.py). It's linked to the DPU VM twice (`dpusim
//! wire`: a bridge on each hypervisor, joined by a VXLAN): its two PFs to the DPU VM's wire NIC,
//! and its tmfifo NIC to the DPU VM's, the rshim link the DPU's agent reaches the host agent over.
//!
//! What makes the host's OS treat its PFs as a BlueField's, and run the simulated hostdriver that
//! hands dpusimaas the bf.cfg, comes from the cluster (openshift-dpf's SIMULATED_DPUS_ONLY).

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use anyhow::{Context, Result, bail};

use crate::{
    command::{output, run, succeeds},
    lease::{Host, Lease},
    manager::Refusal,
};

/// The host VM's disk, as the cluster's root device hint names it.
const ROOT_DEVICE: &str = "/dev/vda";

pub struct HostConfig {
    /// ssh target of the x86 hypervisor (root); none: no host-trusted leases.
    pub hypervisor: Option<String>,
    /// dpusim here (the DPU side of the wire, and to create the host VMs).
    pub dpusim: PathBuf,
    /// dpusim on the x86 hypervisor (its side of the wire).
    pub remote_dpusim: String,
    /// The x86 hypervisor's LAN bridge, for the host's own NIC.
    pub mgmt_bridge: String,
    pub vcpus: u32,
    pub memory_mib: u32,
}

impl HostConfig {
    fn hypervisor(&self) -> Result<&str, Refusal> {
        self.hypervisor.as_deref().ok_or_else(|| {
            Refusal::Invalid(
                "host-trusted DPUs need an x86 hypervisor (dpusimaas --x86-hypervisor)".to_owned(),
            )
        })
    }

    /// Refuses host-trusted leases when there's nowhere to run their hosts.
    pub fn check(&self) -> Result<(), Refusal> {
        self.hypervisor().map(drop)
    }

    pub fn libvirt_uri(&self) -> Result<String> {
        Ok(format!("qemu+ssh://{}/system", self.hypervisor()?))
    }

    async fn virsh(&self, args: &[&str]) -> Result<String> {
        let uri = self.libvirt_uri()?;
        output("virsh", [&["-c", uri.as_str()], args].concat()).await
    }

    async fn host_vm_exists(&self, lease: &Lease) -> Result<bool> {
        let uri = self.libvirt_uri()?;
        Ok(succeeds("virsh", ["-c", &uri, "dominfo", &lease.host_vm()]).await)
    }

    /// Creates the lease's wire and host VM, whichever don't exist yet, and returns the host.
    pub async fn ensure(&self, lease: &Lease) -> Result<Host> {
        self.links_up(lease).await?;
        if !self.host_vm_exists(lease).await? {
            self.create_host_vm(lease).await?;
        }
        Ok(Host {
            vm: lease.host_vm(),
            boot_mac: self.boot_mac(lease).await?,
            root_device: ROOT_DEVICE.to_owned(),
        })
    }

    async fn create_host_vm(&self, lease: &Lease) -> Result<()> {
        let (vcpus, memory) = (self.vcpus.to_string(), self.memory_mib.to_string());
        run(
            &self.dpusim,
            [
                "host-vm",
                "create",
                &lease.host_vm(),
                "--hypervisor",
                self.hypervisor()?,
                "--serial",
                &lease.serial,
                "--mgmt-bridge",
                &self.mgmt_bridge,
                "--wire-bridge",
                &Link::Wire.bridge(&lease.id),
                "--tmfifo-bridge",
                &Link::Tmfifo.bridge(&lease.id),
                "--vcpus",
                &vcpus,
                "--memory",
                &memory,
            ],
        )
        .await
    }

    /// The MAC of the host's NIC on the LAN, as libvirt has it.
    async fn boot_mac(&self, lease: &Lease) -> Result<String> {
        let interfaces = self.virsh(&["domiflist", &lease.host_vm()]).await?;
        interfaces
            .lines()
            .map(str::split_whitespace)
            .find_map(|mut columns| {
                let (_interface, _type, source) =
                    (columns.next()?, columns.next()?, columns.next()?);
                let mac = columns.nth(1)?;
                (source == self.mgmt_bridge).then(|| mac.to_owned())
            })
            .with_context(|| format!("{} has no NIC on {}", lease.host_vm(), self.mgmt_bridge))
    }

    /// The UUID of the host VM, which its Redfish system is named by.
    pub async fn host_uuid(&self, lease: &Lease) -> Result<String> {
        Ok(self
            .virsh(&["domuuid", &lease.host_vm()])
            .await?
            .trim()
            .to_owned())
    }

    pub async fn host_running(&self, lease: &Lease) -> Result<bool> {
        Ok(self.virsh(&["domstate", &lease.host_vm()]).await?.trim() == "running")
    }

    /// The host's power, as its BMC changes it.
    pub async fn host_power(&self, lease: &Lease, virsh_command: &str) -> Result<()> {
        self.virsh(&[virsh_command, &lease.host_vm()])
            .await
            .map(drop)
    }

    /// Removes the host VM (with its disk and virtual media) and the links, as much as it can.
    pub async fn delete(&self, lease: &Lease) -> Result<()> {
        let hypervisor = self.hypervisor()?;
        let mut removals = vec![
            run(
                &self.dpusim,
                [
                    "host-vm",
                    "delete",
                    &lease.host_vm(),
                    "--hypervisor",
                    hypervisor,
                ],
            )
            .await,
        ];
        for link in Link::ALL {
            let bridge = link.bridge(&lease.id);
            removals.push(run(&self.dpusim, ["wire", "down", &bridge]).await);
            removals.push(
                run(
                    "ssh",
                    [hypervisor, &self.remote_dpusim, "wire", "down", &bridge],
                )
                .await,
            );
        }
        let failures: Vec<String> = removals
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        bail!("{}", failures.join("; "))
    }

    /// The lease's links, on both hypervisors (idempotent).
    async fn links_up(&self, lease: &Lease) -> Result<()> {
        let hypervisor = self.hypervisor()?;
        let remote = resolve(hypervisor).await?;
        let local = source_address(remote).await?;
        let (local, remote) = (local.to_string(), remote.to_string());
        for link in Link::ALL {
            let (bridge, vni) = (link.bridge(&lease.id), link.vni(&lease.id)?.to_string());
            run(&self.dpusim, ["wire", "up", &bridge, &vni, &local, &remote]).await?;
            run(
                "ssh",
                [
                    hypervisor,
                    &self.remote_dpusim,
                    "wire",
                    "up",
                    &bridge,
                    &vni,
                    &remote,
                    &local,
                ],
            )
            .await?;
        }
        Ok(())
    }
}

/// A link between a lease's host VM and its DPU VM: a bridge on each hypervisor, joined by a VXLAN.
#[derive(Clone, Copy)]
pub enum Link {
    /// The PCIe side: the host's PFs and the DPU VM's wire NIC.
    Wire,
    /// rshim's tmfifo: the host's and the DPU's tmfifo_net0.
    Tmfifo,
}

impl Link {
    const ALL: [Link; 2] = [Link::Wire, Link::Tmfifo];

    pub fn bridge(self, lease_id: &str) -> String {
        match self {
            Link::Wire => format!("dsw-{lease_id}"),
            Link::Tmfifo => format!("dst-{lease_id}"),
        }
    }

    /// Above the hand-made sim's VNIs, one per lease id (5 hex digits) and link.
    fn vni(self, lease_id: &str) -> Result<u32> {
        let base: u32 = match self {
            Link::Wire => 0x10_0000,
            Link::Tmfifo => 0x20_0000,
        };
        let id = u32::from_str_radix(lease_id, 16)
            .with_context(|| format!("lease id {lease_id:?} isn't hex"))?;
        base.checked_add(id)
            .filter(|vni| *vni < 1 << 24)
            .with_context(|| format!("lease id {lease_id:?} is too big for a VNI"))
    }
}

/// An ssh target's address (`root@host` → host's IPv4).
async fn resolve(ssh_target: &str) -> Result<IpAddr> {
    let host = ssh_target.rsplit('@').next().unwrap_or(ssh_target);
    tokio::net::lookup_host((host, 22))
        .await
        .with_context(|| format!("resolving {host}"))?
        .map(|address: SocketAddr| address.ip())
        .find(IpAddr::is_ipv4)
        .with_context(|| format!("{host} has no IPv4 address"))
}

/// The address this machine reaches `remote` from.
async fn source_address(remote: IpAddr) -> Result<IpAddr> {
    let route = output("ip", ["-o", "route", "get", &remote.to_string()]).await?;
    let mut words = route.split_whitespace();
    words
        .find(|word| *word == "src")
        .and_then(|_src| words.next())
        .and_then(|address| address.parse().ok())
        .with_context(|| format!("no source address in {route:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vni_is_per_lease_and_link_and_fits() {
        assert_eq!(Link::Wire.vni("00000").ok(), Some(0x10_0000));
        assert_eq!(Link::Wire.vni("fffff").ok(), Some(0x1f_ffff));
        assert_eq!(Link::Tmfifo.vni("fffff").ok(), Some(0x2f_ffff));
        assert!(Link::Wire.vni("xyz").is_err());
    }
}
