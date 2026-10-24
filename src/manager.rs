use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::{
    bmc::{self, MockBmc},
    dhcp::{self, DhcpLease},
    fabric,
    host::{HostConfig, Link},
    lease::{Lease, LeaseState, LeaseTerms, Level, Mode, default_cluster, expiry, until_text},
    netns,
    store::LeaseStore,
    vm::{self, DpuHost, VmConfig, VmPowerAction},
};

pub struct ServiceConfig {
    /// Where leases are kept.
    pub state_dir: PathBuf,
    /// The LAN bridge leases get their addresses on.
    pub bridge: String,
    /// Unix socket of the internal API, for the BMCs.
    pub internal_socket: PathBuf,
    /// Python with pyghmi, for the IPMI responders.
    pub python: PathBuf,
    /// The mockbmc binary, for the DPU BMCs.
    pub mockbmc: PathBuf,
    /// Firmware versions a BFB without a software inventory installs (mockbmc's -bfb-versions).
    pub bfb_versions: String,
    pub max_leases: usize,
    pub vm: VmConfig,
    /// Host-trusted leases' hosts.
    pub host: HostConfig,
}

/// A request dpusimaas turns down, and why; the API answers with its status.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("no such DPU")]
    NotFound,
    #[error("no room for another simulated DPU: all {0} are leased")]
    Full(usize),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
}

/// What runs for a lease while dpusimaas does.
#[derive(Clone)]
pub struct LeaseRuntime {
    /// Zero Trust only (a host-trusted lease's host BMC drives the host VM itself).
    pub mock_bmc: Option<MockBmc>,
    /// Serializes changes to the DPU VM.
    pub vm_lock: Arc<tokio::sync::Mutex<()>>,
    /// Stops the lease's address renewal and BMCs.
    pub stop: CancellationToken,
}

/// Owns the leases and what runs for them.
pub struct LeaseManager {
    pub config: ServiceConfig,
    pub store: LeaseStore,
    running: Mutex<HashMap<String, LeaseRuntime>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateRequest {
    owner: Option<String>,
    cluster: Option<String>,
    mode: Option<String>,
    level: Option<String>,
    bmc_password: Option<String>,
    ttl_hours: Option<i64>,
}

impl CreateRequest {
    fn into_terms(self) -> Result<LeaseTerms, Refusal> {
        let owner = self
            .owner
            .filter(|owner| !owner.is_empty())
            .ok_or_else(|| Refusal::Invalid("owner is required".to_owned()))?;
        Ok(LeaseTerms {
            owner,
            cluster: parse_cluster(self.cluster)?,
            mode: parse_mode(self.mode.as_deref().unwrap_or_default())?,
            level: parse_level(self.level.as_deref().unwrap_or_default())?,
            bmc_password: self.bmc_password.filter(|password| !password.is_empty()),
            expires: expiry(self.ttl_hours.unwrap_or_default()),
        })
    }
}

fn parse_cluster(requested: Option<String>) -> Result<String, Refusal> {
    let Some(cluster) = requested.filter(|cluster| !cluster.is_empty()) else {
        return Ok(default_cluster());
    };
    let valid = cluster.len() <= 40
        && cluster
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        return Err(Refusal::Invalid(
            "cluster must be up to 40 lowercase letters, digits and dashes".to_owned(),
        ));
    }
    Ok(cluster)
}

fn parse_mode(requested: &str) -> Result<Mode, Refusal> {
    match requested {
        "" => Ok(Mode::ZeroTrust),
        _ => Mode::ALL
            .into_iter()
            .find(|mode| mode.name() == requested)
            .ok_or_else(|| {
                Refusal::Invalid(format!(
                    "mode must be {} or {}",
                    Mode::ZeroTrust,
                    Mode::HostTrusted
                ))
            }),
    }
}

fn parse_level(requested: &str) -> Result<Level, Refusal> {
    match requested {
        "" => Ok(Level::Vm),
        _ => Level::ALL
            .into_iter()
            .find(|level| level.name() == requested)
            .ok_or_else(|| {
                let names: Vec<&str> = Level::ALL.into_iter().map(Level::name).collect();
                Refusal::Invalid(format!("level must be one of: {}", names.join(", ")))
            }),
    }
}

impl LeaseManager {
    pub fn open(config: ServiceConfig) -> Result<Arc<LeaseManager>> {
        let store = LeaseStore::open(&config.state_dir)?;
        for (script, contents) in [
            (
                bmc::ipmi_script_path(&config.state_dir),
                bmc::IPMI_BMC_SCRIPT,
            ),
            (
                bmc::redfish_script_path(&config.state_dir),
                bmc::REDFISH_BMC_SCRIPT,
            ),
        ] {
            std::fs::write(&script, contents)
                .with_context(|| format!("writing {}", script.display()))?;
        }
        Ok(Arc::new(LeaseManager {
            config,
            store,
            running: Mutex::new(HashMap::new()),
        }))
    }

    /// Brings the stored leases back up after a restart: their namespaces, addresses and BMCs. The
    /// DPU VMs keep running on their own. A restarted mock BMC is back in its factory state, so DPF
    /// sees a DPU that has to be provisioned again if it asks.
    pub async fn restore(self: &Arc<Self>) {
        for lease in self.store.list() {
            if let Err(error) = self.start(&lease).await {
                tracing::error!(lease = lease.id, "restoring the lease: {error:#}");
                let recorded = self.store.update(&lease.id, |lease| {
                    lease.record(format!("couldn't restore after a restart: {error:#}"));
                });
                log_failure(&lease.id, recorded);
            }
        }
    }

    pub async fn create(self: &Arc<Self>, request: CreateRequest) -> Result<Lease> {
        let terms = request.into_terms()?;
        if terms.mode == Mode::HostTrusted {
            self.config.host.check()?;
        }
        let max_leases = self.config.max_leases;
        if self.store.count() >= max_leases {
            return Err(Refusal::Full(max_leases).into());
        }
        let lease = Lease::new(terms);
        self.store.add(lease.clone())?;
        if let Err(error) = self.start(&lease).await {
            if let Err(cleanup_error) = self.destroy(&lease.id).await {
                tracing::error!(
                    lease = lease.id,
                    "ending a lease that didn't start: {cleanup_error:#}"
                );
            }
            return Err(error.context("starting the lease"));
        }
        self.store
            .get(&lease.id)
            .with_context(|| format!("lease {} ended while it started", lease.id))
    }

    /// Gives a lease its namespace, address and BMCs; a host-trusted one also its host VM and
    /// the wire to its DPU. DPF reaches a host-trusted DPU through that host, which hands over the
    /// bf.cfg itself.
    async fn start(self: &Arc<Self>, lease: &Lease) -> Result<()> {
        let runtime = match lease.mode {
            Mode::ZeroTrust => self.start_zero_trust(lease).await?,
            Mode::HostTrusted => self.start_host_trusted(lease).await?,
        };
        self.running.lock().insert(lease.id.clone(), runtime);
        Ok(())
    }

    async fn start_zero_trust(self: &Arc<Self>, lease: &Lease) -> Result<LeaseRuntime> {
        let stop = CancellationToken::new();
        // Whatever started stops again if a later step fails.
        let stop_on_failure: DropGuard = stop.clone().drop_guard();
        netns::ensure(lease, &self.config.bridge).await?;
        let address = dhcp::acquire_address(lease, lease.ip).await?;
        fabric::attach(&self.config.vm.dpusim, lease, &self.config.bridge).await?;
        let mock_bmc = self.start_bmcs(lease, &stop).await?;
        let runtime = LeaseRuntime {
            mock_bmc: Some(mock_bmc.clone()),
            vm_lock: Arc::default(),
            stop,
        };
        self.boot_installed_os(lease, &runtime, &mock_bmc).await;
        self.spawn_address_renewal(&lease.id, address.lease_time, runtime.stop.clone());
        self.store
            .update(&lease.id, |lease| record_address(lease, &address))?;
        stop_on_failure.disarm();
        Ok(runtime)
    }

    async fn start_host_trusted(self: &Arc<Self>, lease: &Lease) -> Result<LeaseRuntime> {
        let stop = CancellationToken::new();
        let stop_on_failure: DropGuard = stop.clone().drop_guard();
        netns::ensure(lease, &self.config.bridge).await?;
        let address = dhcp::acquire_address(lease, lease.ip).await?;
        fabric::attach(&self.config.vm.dpusim, lease, &self.config.bridge).await?;
        let host_config = &self.config.host;
        let host = host_config
            .ensure(lease)
            .await
            .context("creating the host VM")?;
        self.start_host_bmc(lease, &host_config.host_uuid(lease).await?, &stop)
            .await?;
        self.spawn_address_renewal(&lease.id, address.lease_time, stop.clone());
        self.store.update(&lease.id, |lease| {
            if lease.host.is_none() {
                lease.record(format!(
                    "host VM {} (boot MAC {}), powered off until the cluster installs it",
                    host.vm, host.boot_mac
                ));
            }
            lease.host = Some(host);
            record_address(lease, &address);
        })?;
        stop_on_failure.disarm();
        Ok(LeaseRuntime {
            mock_bmc: None,
            vm_lock: Arc::default(),
            stop,
        })
    }

    /// DPF installed an OS before a restart: the VM comes back (the BMC already reports the OS).
    async fn boot_installed_os(
        self: &Arc<Self>,
        lease: &Lease,
        runtime: &LeaseRuntime,
        mock_bmc: &MockBmc,
    ) {
        let Ok(bfcfg) = tokio::fs::read(self.bfcfg_path(&lease.id)).await else {
            return;
        };
        if vm::exists(&lease.vm).await {
            return;
        }
        let manager = Arc::clone(self);
        let lease_id = lease.id.clone();
        let runtime = runtime.clone();
        let mock_bmc = mock_bmc.clone();
        tokio::spawn(async move {
            let booted = manager
                .boot_through_bmc(&runtime, &mock_bmc, &lease_id, &bfcfg)
                .await;
            log_failure(&lease_id, booted);
        });
    }

    pub async fn destroy(&self, lease_id: &str) -> Result<()> {
        let lease = self.store.get(lease_id).ok_or(Refusal::NotFound)?;
        if let Some(runtime) = self.running.lock().remove(lease_id) {
            runtime.stop.cancel();
        }
        if let Err(error) = self.config.vm.delete(&lease).await {
            tracing::error!(lease = lease_id, "deleting the DPU VM: {error:#}");
        }
        if lease.mode == Mode::HostTrusted
            && let Err(error) = self.config.host.delete(&lease).await
        {
            tracing::error!(lease = lease_id, "deleting the host VM: {error:#}");
        }
        if let Err(error) = fabric::detach(&self.config.vm.dpusim, &lease).await {
            tracing::error!(lease = lease_id, "detaching from the leaf: {error:#}");
        }
        netns::delete(lease_id).await;
        let work_dir = self.work_dir(lease_id);
        if work_dir.exists() {
            tokio::fs::remove_dir_all(&work_dir)
                .await
                .with_context(|| format!("removing {}", work_dir.display()))?;
        }
        let mock_bmc_socket = self.mock_bmc_socket(lease_id);
        if mock_bmc_socket.exists() {
            tokio::fs::remove_file(&mock_bmc_socket)
                .await
                .with_context(|| format!("removing {}", mock_bmc_socket.display()))?;
        }
        self.store.remove(lease_id)?;
        tracing::info!(lease = lease_id, owner = lease.owner, "lease ended");
        Ok(())
    }

    pub fn renew(&self, lease_id: &str, ttl_hours: i64) -> Result<Lease> {
        let renewed = self.store.update(lease_id, |lease| {
            lease.expires = expiry(ttl_hours);
            match lease.expires {
                None => lease.record("no longer ends on its own"),
                Some(_) => lease.record(format!("now ends{}", until_text(lease.expires))),
            }
        })?;
        Ok(renewed.ok_or(Refusal::NotFound)?)
    }

    pub fn work_dir(&self, lease_id: &str) -> PathBuf {
        self.config.state_dir.join("leases").join(lease_id)
    }

    /// Where the bf.cfg of the OS DPF installed is kept.
    pub fn bfcfg_path(&self, lease_id: &str) -> PathBuf {
        self.work_dir(lease_id).join("bf.cfg")
    }

    /// (Re)creates the DPU VM from the bf.cfg DPF installed.
    /// A host-trusted DPU's host says its node name (`host_node`), when it knows it.
    pub async fn boot(
        &self,
        lease_id: &str,
        bfcfg: &[u8],
        host_node: Option<String>,
    ) -> Result<()> {
        let lease = self
            .store
            .update(lease_id, |lease| {
                lease.state = LeaseState::Booting;
                lease.record(format!(
                    "DPF installed an OS ({}-byte bf.cfg); creating the DPU VM",
                    bfcfg.len()
                ));
            })?
            .ok_or(Refusal::NotFound)?;
        let dpu_host = (lease.mode == Mode::HostTrusted).then(|| DpuHost {
            wire_bridge: Link::Wire.bridge(lease_id),
            tmfifo_bridge: Link::Tmfifo.bridge(lease_id),
            node: host_node,
        });
        let created = self
            .config
            .vm
            .create(&lease, bfcfg, &self.work_dir(lease_id), dpu_host.as_ref())
            .await;
        self.store.update(lease_id, |lease| match &created {
            Ok(()) => {
                lease.state = LeaseState::Running;
                lease.record(format!("DPU VM {} is booting", lease.vm));
            }
            Err(error) => {
                lease.state = LeaseState::Failed;
                lease.record(format!("creating the DPU VM failed: {error:#}"));
            }
        })?;
        created.context("creating the DPU VM")
    }

    /// Applies a power action to the lease's DPU VM and records it.
    pub async fn vm_action(&self, lease_id: &str, description: &str, action: VmPowerAction) {
        let Some(lease) = self.store.get(lease_id) else {
            return;
        };
        let applied = action.apply(&lease.vm).await;
        let recorded = self.store.update(lease_id, |lease| match &applied {
            Ok(()) => lease.record(description),
            Err(error) => lease.record(format!("{description}: {error:#}")),
        });
        log_failure(lease_id, recorded);
    }

    pub fn runtime(&self, lease_id: &str) -> Option<LeaseRuntime> {
        self.running.lock().get(lease_id).cloned()
    }

    /// Ends expired leases, every minute until `stop`.
    pub async fn reap(&self, stop: CancellationToken) {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(Duration::from_mins(1)) => {}
            }
            for lease in self.store.list().into_iter().filter(Lease::has_expired) {
                tracing::info!(lease = lease.id, owner = lease.owner, "lease expired");
                log_failure(&lease.id, self.destroy(&lease.id).await);
            }
        }
    }

    fn spawn_address_renewal(
        self: &Arc<Self>,
        lease_id: &str,
        lease_time: Duration,
        stop: CancellationToken,
    ) {
        let manager = Arc::clone(self);
        let lease_id = lease_id.to_owned();
        tokio::spawn(async move {
            tokio::select! {
                () = stop.cancelled() => {}
                () = manager.keep_address(&lease_id, lease_time) => {}
            }
        });
    }

    /// Renews the lease's address at half its lease time, for as long as the lease exists.
    async fn keep_address(&self, lease_id: &str, first_lease_time: Duration) {
        let mut lease_time = first_lease_time;
        loop {
            tokio::time::sleep(lease_time.div_f64(2.0)).await;
            let Some(lease) = self.store.get(lease_id) else {
                return;
            };
            lease_time = match dhcp::acquire_address(&lease, lease.ip).await {
                Ok(renewed) => {
                    let moved = self
                        .store
                        .update(lease_id, |lease| record_move(lease, &renewed));
                    log_failure(lease_id, moved);
                    renewed.lease_time
                }
                Err(error) => {
                    tracing::error!(lease = lease_id, "renewing the address: {error:#}");
                    // Retry soon: in half of this.
                    Duration::from_mins(2)
                }
            };
        }
    }
}

fn mark_waiting(lease: &mut Lease) {
    if lease.state == LeaseState::Starting {
        lease.state = LeaseState::Waiting;
    }
}

fn record_address(lease: &mut Lease, address: &DhcpLease) {
    if lease.ip != Some(address.address) {
        lease.record(format!(
            "DHCP gave {}; its BMCs answer there (Redfish 443, IPMI 623)",
            address.address
        ));
    }
    lease.ip = Some(address.address);
    mark_waiting(lease);
}

fn record_move(lease: &mut Lease, renewed: &DhcpLease) {
    if lease.ip == Some(renewed.address) {
        return;
    }
    let previous = lease.ip.map(|ip| ip.to_string()).unwrap_or_default();
    lease.record(format!(
        "DHCP moved the lease from {previous} to {}",
        renewed.address
    ));
    lease.ip = Some(renewed.address);
}

/// Logs what failed in work nobody waits for.
pub fn log_failure<T>(lease_id: &str, outcome: Result<T>) {
    if let Err(error) = outcome {
        tracing::error!(lease = lease_id, "{error:#}");
    }
}

pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
