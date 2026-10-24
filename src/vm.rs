//! The DPU itself is an aarch64 VM that boots with the ignition DPF put in the bf.cfg it installed.
//! dpusim owns all of that (converting the bf.cfg, the disk, the VM); dpusimaas only says when.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{
    command::{output, run, succeeds},
    lease::Lease,
};

/// How a host-trusted DPU VM is linked to its host VM.
pub struct DpuHost {
    pub wire_bridge: String,
    pub tmfifo_bridge: String,
    /// The host's node name, for the host PF MAC OVN-K's simulated-DPU mode expects.
    pub node: Option<String>,
}

pub struct VmConfig {
    /// The dpusim binary.
    pub dpusim: PathBuf,
    /// LAN bridge for the VM's management NIC.
    pub bridge: String,
    pub vcpus: u32,
    pub memory_mib: u32,
}

/// A power change of the DPU's Arm.
#[derive(Clone, Copy, Debug)]
pub enum VmPowerAction {
    /// A running VM resets, a stopped one starts.
    Reset,
    PowerOn,
    PowerOff,
}

impl VmPowerAction {
    pub async fn apply(self, vm_name: &str) -> Result<()> {
        if !exists(vm_name).await {
            return Ok(());
        }
        let virsh_command = match (self, is_running(vm_name).await) {
            (VmPowerAction::Reset, true) => "reset",
            (VmPowerAction::Reset | VmPowerAction::PowerOn, false) => "start",
            (VmPowerAction::PowerOff, true) => "destroy",
            (VmPowerAction::PowerOn, true) | (VmPowerAction::PowerOff, false) => return Ok(()),
        };
        run("virsh", [virsh_command, vm_name]).await
    }
}

/// What dpusim names the DPU by (its VM NIC MACs derive from it).
fn dpu_id(lease: &Lease) -> String {
    format!("sim-{}", lease.id)
}

pub async fn exists(vm_name: &str) -> bool {
    succeeds("virsh", ["dominfo", vm_name]).await
}

async fn is_running(vm_name: &str) -> bool {
    output("virsh", ["domstate", vm_name])
        .await
        .is_ok_and(|state| state.trim() == "running")
}

impl VmConfig {
    /// (Re)creates the DPU VM from a freshly installed bf.cfg: a new disk, so a clean install. The
    /// bf.cfg is kept in the lease's work directory, to boot it again after a restart.
    /// A host-trusted DPU VM is linked to its host.
    pub async fn create(
        &self,
        lease: &Lease,
        bfcfg: &[u8],
        work_dir: &Path,
        host: Option<&DpuHost>,
    ) -> Result<()> {
        tokio::fs::create_dir_all(work_dir)
            .await
            .with_context(|| format!("creating {}", work_dir.display()))?;
        let bfcfg_path = work_dir.join("bf.cfg");
        tokio::fs::write(&bfcfg_path, bfcfg)
            .await
            .with_context(|| format!("writing {}", bfcfg_path.display()))?;
        self.delete(lease).await?;
        let dpu = dpu_id(lease);
        let vcpus = self.vcpus.to_string();
        let memory = self.memory_mib.to_string();
        let bfcfg_arg = bfcfg_path.to_string_lossy();
        let mut args: Vec<&str> = vec![
            "dpu-vm",
            "create",
            &dpu,
            "--name",
            &lease.vm,
            "--hypervisor",
            "local",
            "--bfcfg",
            &bfcfg_arg,
            "--serial",
            &lease.serial,
            "--mgmt-bridge",
            &self.bridge,
            "--vcpus",
            &vcpus,
            "--memory",
            &memory,
        ];
        if let Some(host) = host {
            args.extend([
                "--wire-bridge",
                &host.wire_bridge,
                "--tmfifo-bridge",
                &host.tmfifo_bridge,
            ]);
            if let Some(node) = &host.node {
                args.extend(["--host-node", node]);
            }
        }
        run(&self.dpusim, args).await
    }

    /// Removes the VM, its disk and its ignition.
    pub async fn delete(&self, lease: &Lease) -> Result<()> {
        if !exists(&lease.vm).await {
            return Ok(());
        }
        let dpu = dpu_id(lease);
        run(
            &self.dpusim,
            [
                "dpu-vm",
                "delete",
                &dpu,
                "--name",
                &lease.vm,
                "--hypervisor",
                "local",
            ],
        )
        .await
    }
}
