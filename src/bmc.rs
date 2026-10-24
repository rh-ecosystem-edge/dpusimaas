//! A lease's BMCs, all on the lease's address. Zero Trust:
//!   - the DPU's BMC: mockbmc (mockbmc/), upstream DPF's mock BlueField Redfish server as is. When
//!     DPF finishes installing a BFB, mockbmc hands dpusimaas the bf.cfg and dpusimaas boots the VM.
//!   - the host's BMC: IPMI lanplus (pyghmi, ipmi_bmc.py), what DPF's DPUNode reboot Job and the
//!     automation's power-on use. A host reset resets the DPU VM, as it does a real DPU.
//!
//! Host-trusted, the host's BMC only, for its host VM: Redfish with virtual media (sushy-tools,
//! redfish_bmc.py), which the cluster installs the host through, and the same IPMI.
//!
//! dpusimaas keeps them running, and tells mockbmc whenever the DPU's power changes.

use std::{future::Future, path::PathBuf, process::ExitStatus, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use hyper::Method;
use tokio::{
    process::{Child, Command},
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;

use crate::{
    command::describe,
    lease::{Lease, LeaseState, Mode},
    manager::{LeaseManager, LeaseRuntime, Refusal, log_failure, path_text},
    netns, unix_http,
    vm::{self, VmPowerAction},
};

pub const IPMI_BMC_SCRIPT: &[u8] = include_bytes!("ipmi_bmc.py");
pub const REDFISH_BMC_SCRIPT: &[u8] = include_bytes!("redfish_bmc.py");

const RESTART_DELAY: Duration = Duration::from_secs(5);
const MOCK_BMC_START_TIMEOUT: Duration = Duration::from_secs(30);
const REDFISH_START_TIMEOUT: Duration = Duration::from_mins(1);

pub fn ipmi_script_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("ipmi_bmc.py")
}

pub fn redfish_script_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("redfish_bmc.py")
}

/// A lease's running mockbmc, as dpusimaas reaches it.
#[derive(Clone)]
pub struct MockBmc {
    lease_id: String,
    control_socket: PathBuf,
}

/// A change in the DPU's power that the mock BMC reports.
#[derive(Clone, Copy, Debug)]
pub enum MockStateChange {
    PowerOn,
    PowerOff,
    OsStarting,
    /// Firmware DPF activated takes effect on a host reboot.
    ApplyActivatedBundle,
}

impl MockStateChange {
    fn name(self) -> &'static str {
        match self {
            MockStateChange::PowerOn => "power-on",
            MockStateChange::PowerOff => "power-off",
            MockStateChange::OsStarting => "os-starting",
            MockStateChange::ApplyActivatedBundle => "apply-activated-bundle",
        }
    }
}

impl MockBmc {
    /// Tells the mock BMC; a mock that can't be told (it's restarting) is logged, not fatal: the
    /// VM already changed, and a restarted mock starts over anyway.
    pub async fn tell(&self, change: MockStateChange) {
        let path = format!("/state/{}", change.name());
        if let Err(error) = unix_http::send(&self.control_socket, Method::POST, &path).await {
            tracing::warn!(
                lease = self.lease_id,
                "telling the mock BMC {}: {error:#}",
                change.name()
            );
        }
    }
}

/// The host's BMC being asked to change (or report) the host's power: IPMI chassis control.
#[derive(Clone, Copy, Debug)]
pub enum HostPowerAction {
    On,
    Off,
    Reset,
    Cycle,
    Status,
}

impl HostPowerAction {
    pub fn parse(requested: &str) -> Result<HostPowerAction, Refusal> {
        match requested {
            "on" => Ok(HostPowerAction::On),
            "off" => Ok(HostPowerAction::Off),
            "reset" => Ok(HostPowerAction::Reset),
            "cycle" => Ok(HostPowerAction::Cycle),
            "status" => Ok(HostPowerAction::Status),
            _ => Err(Refusal::Invalid(format!(
                "unknown power action {requested:?}"
            ))),
        }
    }
}

impl LeaseManager {
    /// mockbmc's control socket, next to dpusimaas's own (on /run: a socket path is at most 108
    /// bytes, and a state directory can be anywhere).
    pub fn mock_bmc_socket(&self, lease_id: &str) -> PathBuf {
        self.config
            .internal_socket
            .with_file_name(format!("dpusimaas-mockbmc-{lease_id}.sock"))
    }

    /// Starts the lease's BMCs and keeps them running until `stop`.
    pub async fn start_bmcs(
        self: &Arc<Self>,
        lease: &Lease,
        stop: &CancellationToken,
    ) -> Result<MockBmc> {
        let mock_bmc_process = self.spawn_mock_bmc(lease).await?;
        let ipmi_process = self.spawn_ipmi_responder(lease)?;
        let respawn_mock_bmc = {
            let manager = Arc::clone(self);
            let lease = lease.clone();
            move || {
                let manager = Arc::clone(&manager);
                let lease = lease.clone();
                async move { manager.spawn_mock_bmc(&lease).await }
            }
        };
        let respawn_ipmi_responder = {
            let manager = Arc::clone(self);
            let lease = lease.clone();
            move || std::future::ready(manager.spawn_ipmi_responder(&lease))
        };
        tokio::spawn(supervise(
            "mock BMC",
            lease.id.clone(),
            stop.clone(),
            mock_bmc_process,
            respawn_mock_bmc,
        ));
        tokio::spawn(supervise(
            "IPMI responder",
            lease.id.clone(),
            stop.clone(),
            ipmi_process,
            respawn_ipmi_responder,
        ));
        Ok(MockBmc {
            lease_id: lease.id.clone(),
            control_socket: self.mock_bmc_socket(&lease.id),
        })
    }

    /// Starts a host-trusted lease's host BMC (Redfish and IPMI), waits for its Redfish, and keeps
    /// it running until `stop`.
    pub async fn start_host_bmc(
        self: &Arc<Self>,
        lease: &Lease,
        host_uuid: &str,
        stop: &CancellationToken,
    ) -> Result<()> {
        let mut redfish_process = self.spawn_redfish_responder(lease, host_uuid)?;
        wait_until_redfish_listens(&mut redfish_process, &lease.id)
            .await
            .context("starting the host's Redfish")?;
        let ipmi_process = self.spawn_ipmi_responder(lease)?;
        let respawn_redfish_responder = {
            let manager = Arc::clone(self);
            let lease = lease.clone();
            let host_uuid = host_uuid.to_owned();
            move || std::future::ready(manager.spawn_redfish_responder(&lease, &host_uuid))
        };
        let respawn_ipmi_responder = {
            let manager = Arc::clone(self);
            let lease = lease.clone();
            move || std::future::ready(manager.spawn_ipmi_responder(&lease))
        };
        tokio::spawn(supervise(
            "Redfish responder",
            lease.id.clone(),
            stop.clone(),
            redfish_process,
            respawn_redfish_responder,
        ));
        tokio::spawn(supervise(
            "IPMI responder",
            lease.id.clone(),
            stop.clone(),
            ipmi_process,
            respawn_ipmi_responder,
        ));
        Ok(())
    }

    /// Starts the host's Redfish (sushy-tools) in the lease's namespace.
    fn spawn_redfish_responder(&self, lease: &Lease, host_uuid: &str) -> Result<Child> {
        let script = redfish_script_path(&self.config.state_dir);
        let mut command = Command::new("ip");
        command
            .args(["netns", "exec", &netns::namespace_name(&lease.id)])
            .arg(&self.config.python)
            .arg(&script)
            .env("LIBVIRT_URI", self.config.host.libvirt_uri()?)
            .env("HOST_VM", lease.host_vm())
            .env("HOST_UUID", host_uuid)
            .env("BMC_USER", &lease.ipmi_user)
            .env("BMC_PASSWORD", &lease.ipmi_password)
            .env("STATE_DIR", self.work_dir(&lease.id).join("redfish"))
            .kill_on_drop(true);
        command
            .spawn()
            .with_context(|| format!("starting {}", describe(&command)))
    }

    /// Starts mockbmc for the lease and waits for it to listen. A BMC whose DPU had an OS
    /// installed (it has a bf.cfg) reports it again: the mock keeps no state across restarts.
    async fn spawn_mock_bmc(&self, lease: &Lease) -> Result<Child> {
        let control_socket = self.mock_bmc_socket(&lease.id);
        let installed = tokio::fs::try_exists(self.bfcfg_path(&lease.id))
            .await
            .unwrap_or(false);
        let mut command = Command::new(&self.config.mockbmc);
        command
            .args([
                "-lease",
                &lease.id,
                "-netns",
                &netns::namespace_name(&lease.id),
            ])
            .args([
                "-serial",
                &lease.serial,
                "-bfb-versions",
                &self.config.bfb_versions,
            ])
            .args(["-control", &path_text(&control_socket)])
            .args([
                "-dpusimaas-socket",
                &path_text(&self.config.internal_socket),
            ])
            .args(installed.then_some("-installed"))
            .kill_on_drop(true);
        let mut process = command
            .spawn()
            .with_context(|| format!("starting {}", describe(&command)))?;
        wait_until_listening(&mut process, &control_socket)
            .await
            .context("starting the mock BMC")?;
        Ok(process)
    }

    /// Starts the host's IPMI responder in the lease's namespace.
    fn spawn_ipmi_responder(&self, lease: &Lease) -> Result<Child> {
        let script = ipmi_script_path(&self.config.state_dir);
        let mut command = Command::new("ip");
        command
            .args(["netns", "exec", &netns::namespace_name(&lease.id)])
            .arg(&self.config.python)
            .arg(&script)
            .env("LEASE", &lease.id)
            .env("SOCKET", &self.config.internal_socket)
            .env("IPMI_USER", &lease.ipmi_user)
            .env("IPMI_PASSWORD", &lease.ipmi_password)
            .kill_on_drop(true);
        command
            .spawn()
            .with_context(|| format!("starting {}", describe(&command)))
    }

    fn runtime_with_bmc(&self, lease_id: &str) -> Result<(LeaseRuntime, MockBmc), Refusal> {
        self.runtime(lease_id)
            .and_then(|runtime| runtime.mock_bmc.clone().map(|mock_bmc| (runtime, mock_bmc)))
            .ok_or(Refusal::NotFound)
    }

    /// The host's BMC changing the host's power. The DPU sits in the host, so it goes with it.
    pub async fn host_power(
        &self,
        lease_id: &str,
        action: HostPowerAction,
    ) -> Result<&'static str> {
        let lease = self.store.get(lease_id).ok_or(Refusal::NotFound)?;
        if lease.mode == Mode::HostTrusted {
            return self.host_vm_power(&lease, action).await;
        }
        let (runtime, mock_bmc) = self.runtime_with_bmc(lease_id)?;
        if let HostPowerAction::Status = action {
            return Ok(match lease.state {
                LeaseState::PoweredOff => "off",
                _ => "on",
            });
        }
        let _vm_change = runtime.vm_lock.lock().await;
        match action {
            HostPowerAction::On => {
                self.vm_action(lease_id, "host powered on", VmPowerAction::PowerOn)
                    .await;
                self.leave_powered_off(lease_id).await?;
                mock_bmc.tell(MockStateChange::PowerOn).await;
            }
            HostPowerAction::Off => {
                self.vm_action(lease_id, "host powered off", VmPowerAction::PowerOff)
                    .await;
                self.store
                    .update(lease_id, |lease| lease.state = LeaseState::PoweredOff)?;
                mock_bmc.tell(MockStateChange::PowerOff).await;
            }
            HostPowerAction::Reset | HostPowerAction::Cycle => {
                let description = match action {
                    HostPowerAction::Cycle => "host cycle",
                    _ => "host reset",
                };
                mock_bmc.tell(MockStateChange::ApplyActivatedBundle).await;
                mock_bmc.tell(MockStateChange::OsStarting).await;
                self.vm_action(lease_id, description, VmPowerAction::Reset)
                    .await;
                self.leave_powered_off(lease_id).await?;
                mock_bmc.tell(MockStateChange::PowerOn).await;
            }
            HostPowerAction::Status => {}
        }
        Ok("ok")
    }

    /// A host-trusted host's BMC changing the host VM's power. The DPU VM isn't touched: DPF
    /// resets the DPU itself (through the host) when it needs to.
    async fn host_vm_power(&self, lease: &Lease, action: HostPowerAction) -> Result<&'static str> {
        let host = &self.config.host;
        let running = host.host_running(lease).await?;
        let virsh_commands: &[&str] = match (action, running) {
            (HostPowerAction::Status, true) => return Ok("on"),
            (HostPowerAction::Status, false) => return Ok("off"),
            (HostPowerAction::On, true) | (HostPowerAction::Off, false) => &[],
            (HostPowerAction::On | HostPowerAction::Reset | HostPowerAction::Cycle, false) => {
                &["start"]
            }
            (HostPowerAction::Off, true) => &["destroy"],
            (HostPowerAction::Reset, true) => &["reset"],
            (HostPowerAction::Cycle, true) => &["destroy", "start"],
        };
        for virsh_command in virsh_commands {
            host.host_power(lease, virsh_command).await?;
        }
        let recorded = self.store.update(&lease.id, |lease| {
            lease.record(format!("host {action:?} over IPMI").to_lowercase());
        });
        log_failure(&lease.id, recorded);
        Ok("ok")
    }

    /// A powered-off lease powering on runs its VM again, or waits for DPF if it has none.
    async fn leave_powered_off(&self, lease_id: &str) -> Result<()> {
        let Some(lease) = self.store.get(lease_id) else {
            return Ok(());
        };
        let powered_on_state = if vm::exists(&lease.vm).await {
            LeaseState::Running
        } else {
            LeaseState::Waiting
        };
        self.store.update(lease_id, |lease| {
            if lease.state == LeaseState::PoweredOff {
                lease.state = powered_on_state;
            }
        })?;
        Ok(())
    }

    /// DPF installed a BFB through the lease's BMC: boot the DPU VM from its bf.cfg.
    pub async fn dpu_installed(&self, lease_id: &str, bfcfg: &[u8]) -> Result<()> {
        let (runtime, mock_bmc) = self.runtime_with_bmc(lease_id)?;
        self.boot_through_bmc(&runtime, &mock_bmc, lease_id, bfcfg)
            .await
    }

    pub async fn boot_through_bmc(
        &self,
        runtime: &LeaseRuntime,
        mock_bmc: &MockBmc,
        lease_id: &str,
        bfcfg: &[u8],
    ) -> Result<()> {
        let _vm_change = runtime.vm_lock.lock().await;
        mock_bmc.tell(MockStateChange::OsStarting).await;
        let booted = self.boot(lease_id, bfcfg, None).await;
        mock_bmc.tell(MockStateChange::PowerOn).await;
        booted
    }

    /// DPF reset the DPU's Arm through its BMC.
    pub async fn dpu_reset(&self, lease_id: &str) -> Result<()> {
        let (runtime, mock_bmc) = self.runtime_with_bmc(lease_id)?;
        let _vm_change = runtime.vm_lock.lock().await;
        mock_bmc.tell(MockStateChange::OsStarting).await;
        self.vm_action(lease_id, "DPF reset the DPU's Arm", VmPowerAction::Reset)
            .await;
        mock_bmc.tell(MockStateChange::PowerOn).await;
        Ok(())
    }
}

/// Waits for the host's Redfish to accept connections on 443 in the lease's namespace.
async fn wait_until_redfish_listens(process: &mut Child, lease_id: &str) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(REDFISH_START_TIMEOUT)
        .context("computing the start deadline")?;
    loop {
        if let Some(status) = process
            .try_wait()
            .context("checking on the Redfish responder")?
        {
            bail!("it exited ({status}) before it was up; its output is in the journal");
        }
        let listening = netns::run_in(lease_id, || {
            Ok(std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, 443)).is_ok())
        })
        .await?;
        if listening {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("it isn't up after {}s", REDFISH_START_TIMEOUT.as_secs());
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// Waits for mockbmc to listen on its control socket, which it does once Redfish is up.
async fn wait_until_listening(process: &mut Child, control_socket: &std::path::Path) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(MOCK_BMC_START_TIMEOUT)
        .context("computing the start deadline")?;
    loop {
        if let Some(status) = process.try_wait().context("checking on mockbmc")? {
            bail!("mockbmc exited ({status}) before it was up; its output is in the journal");
        }
        if unix_http::send(control_socket, Method::GET, "/healthz")
            .await
            .is_ok()
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "mockbmc isn't up after {}s",
                MOCK_BMC_START_TIMEOUT.as_secs()
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Keeps a lease's process running until `stop`: restarts it when it exits.
async fn supervise<Respawn, Respawning>(
    what: &'static str,
    lease_id: String,
    stop: CancellationToken,
    first_process: Child,
    respawn: Respawn,
) where
    Respawn: Fn() -> Respawning,
    Respawning: Future<Output = Result<Child>>,
{
    let mut process = first_process;
    loop {
        let exited = tokio::select! {
            () = stop.cancelled() => {
                stop_process(what, &lease_id, &mut process).await;
                return;
            }
            exited = process.wait() => exited,
        };
        tracing::error!(
            lease = lease_id,
            "{what} exited ({}); restarting",
            describe_exit(exited)
        );
        let Some(restarted) = restart(what, &lease_id, &stop, &respawn).await else {
            return;
        };
        process = restarted;
    }
}

/// Starts a process again, retrying until it starts; none once `stop`.
async fn restart<Respawn, Respawning>(
    what: &'static str,
    lease_id: &str,
    stop: &CancellationToken,
    respawn: &Respawn,
) -> Option<Child>
where
    Respawn: Fn() -> Respawning,
    Respawning: Future<Output = Result<Child>>,
{
    loop {
        tokio::select! {
            () = stop.cancelled() => return None,
            () = sleep(RESTART_DELAY) => {}
        }
        match respawn().await {
            Ok(process) => return Some(process),
            Err(error) => tracing::error!(lease = lease_id, "restarting the {what}: {error:#}"),
        }
    }
}

async fn stop_process(what: &str, lease_id: &str, process: &mut Child) {
    if let Err(error) = process.kill().await {
        tracing::warn!(lease = lease_id, "stopping the {what}: {error}");
    }
}

fn describe_exit(exited: std::io::Result<ExitStatus>) -> String {
    match exited {
        Ok(status) => status.to_string(),
        Err(error) => format!("waiting for it failed: {error}"),
    }
}
