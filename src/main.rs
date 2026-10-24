//! dpusimaas leases simulated BlueField DPUs on an aarch64 hypervisor. A lease is a DPU's BMC (and
//! its host's) on an address of its own from the lab's DHCP; once DPF installs an OS on it, a real
//! aarch64 VM boots as that DPU. A host-trusted lease's host is an x86 VM on an x86 hypervisor,
//! wired to its DPU VM. See README.md.

mod api;
mod bmc;
mod command;
mod dhcp;
mod fabric;
mod host;
mod lease;
mod manager;
mod netns;
mod store;
mod unix_http;
mod vm;

use std::{io::IsTerminal, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::{
    net::{TcpListener, UnixListener},
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;

use crate::{
    host::HostConfig,
    manager::{LeaseManager, ServiceConfig},
    vm::VmConfig,
};

/// What openshift-dpf's RHCOS BFB installs (manifests/post-installation/common/bfb.yaml).
const RHCOS_BFB_VERSIONS: &str =
    "atf=4.15.0-4-g419fbf393,bsp=4.15.0.13998,uefi=4.15.0-19-g37c6f5adb2,doca=3.4.1";

#[derive(Parser)]
#[command(about = "Leases simulated BlueField DPUs")]
struct DpusimaasArgs {
    /// Address of the public API (":80" is every address)
    #[arg(long, default_value = ":80")]
    listen: String,
    /// Where leases are kept
    #[arg(long, default_value = "/var/lib/dpusimaas")]
    state_dir: PathBuf,
    /// The LAN bridge leases get their addresses on
    #[arg(long, default_value = "mgmt-br")]
    bridge: String,
    /// Unix socket of the internal API
    #[arg(long, default_value = "/run/dpusimaas.sock")]
    socket: PathBuf,
    /// Python with pyghmi, for the IPMI responders
    #[arg(long, default_value = "/opt/dpusimaas/venv/bin/python")]
    python: PathBuf,
    /// The mockbmc binary, for the DPU BMCs
    #[arg(long, default_value = "/opt/dpusimaas/mockbmc")]
    mockbmc: PathBuf,
    /// How many simulated DPUs can be leased at once
    #[arg(long = "max-dpus", default_value_t = 4)]
    max_dpus: usize,
    /// The dpusim binary
    #[arg(long, default_value = "/usr/local/bin/dpusim")]
    dpusim: PathBuf,
    /// vCPUs per DPU VM
    #[arg(long, default_value_t = 8)]
    vm_vcpus: u32,
    /// Memory per DPU VM, MiB
    #[arg(long, default_value_t = 16384)]
    vm_memory: u32,
    /// Firmware versions a BFB without a software inventory installs: atf=,bsp=,uefi=,doca=
    #[arg(long, default_value = RHCOS_BFB_VERSIONS)]
    bfb_versions: String,
    /// ssh target (root) of the x86 hypervisor for host-trusted leases' hosts; none: zero-trust only
    #[arg(long)]
    x86_hypervisor: Option<String>,
    /// The dpusim binary on the x86 hypervisor
    #[arg(long, default_value = "/opt/dpusimaas/dpusim")]
    x86_dpusim: String,
    /// The x86 hypervisor's LAN bridge, for the hosts' own NICs
    #[arg(long, default_value = "br0")]
    x86_bridge: String,
    /// vCPUs per host VM
    #[arg(long, default_value_t = 8)]
    host_vcpus: u32,
    /// Memory per host VM, MiB
    #[arg(long, default_value_t = 16384)]
    host_memory: u32,
}

impl DpusimaasArgs {
    fn into_config(self) -> ServiceConfig {
        ServiceConfig {
            host: HostConfig {
                hypervisor: self.x86_hypervisor,
                dpusim: self.dpusim.clone(),
                remote_dpusim: self.x86_dpusim,
                mgmt_bridge: self.x86_bridge,
                vcpus: self.host_vcpus,
                memory_mib: self.host_memory,
            },
            vm: VmConfig {
                dpusim: self.dpusim,
                bridge: self.bridge.clone(),
                vcpus: self.vm_vcpus,
                memory_mib: self.vm_memory,
            },
            state_dir: self.state_dir,
            bridge: self.bridge,
            internal_socket: self.socket,
            python: self.python,
            mockbmc: self.mockbmc,
            bfb_versions: self.bfb_versions,
            max_leases: self.max_dpus,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();
    let args = DpusimaasArgs::parse();
    let public_address = listen_address(&args.listen);
    let config = args.into_config();
    check_bfb_versions(&config).await?;
    let manager = LeaseManager::open(config)?;

    serve_internal_api(&manager).await?;
    manager.restore().await;
    let stop = CancellationToken::new();
    let reaper = Arc::clone(&manager);
    tokio::spawn({
        let stop = stop.clone();
        async move { reaper.reap(stop).await }
    });
    serve_public_api(&manager, &public_address, stop).await
}

/// A Go-style listen address (":80") as one tokio binds: every address, IPv4 and IPv6.
fn listen_address(listen: &str) -> String {
    match listen.strip_prefix(':') {
        Some(port) => format!("[::]:{port}"),
        None => listen.to_owned(),
    }
}

/// mockbmc reads --bfb-versions; dpusimaas has it check them now rather than at the first lease.
async fn check_bfb_versions(config: &ServiceConfig) -> Result<()> {
    let mockbmc = config.mockbmc.to_string_lossy();
    command::run(&*mockbmc, ["-check", "-bfb-versions", &config.bfb_versions])
        .await
        .context("checking --bfb-versions")
}

async fn serve_internal_api(manager: &Arc<LeaseManager>) -> Result<()> {
    let socket = &manager.config.internal_socket;
    if socket.exists() {
        tokio::fs::remove_file(socket)
            .await
            .with_context(|| format!("removing the old {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    let router = api::internal_router(Arc::clone(manager));
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!("serving the internal API: {error}");
        }
    });
    Ok(())
}

async fn serve_public_api(
    manager: &Arc<LeaseManager>,
    address: &str,
    stop: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("listening on {address}"))?;
    let router =
        api::public_router(Arc::clone(manager)).into_make_service_with_connect_info::<SocketAddr>();
    let serving = axum::serve(listener, router)
        .with_graceful_shutdown(stop.clone().cancelled_owned())
        .into_future();
    tracing::info!(
        addr = address,
        max_dpus = manager.config.max_leases,
        "dpusimaas listening"
    );
    let mut public_api = tokio::spawn(serving);
    tokio::select! {
        signalled = termination() => signalled?,
        stopped = &mut public_api => return api_outcome(stopped),
    }
    stop.cancel();
    // Requests in flight get a few seconds to finish.
    match tokio::time::timeout(Duration::from_secs(5), public_api).await {
        Ok(stopped) => api_outcome(stopped),
        Err(_) => Ok(()),
    }
}

fn api_outcome(stopped: Result<std::io::Result<()>, tokio::task::JoinError>) -> Result<()> {
    stopped
        .context("waiting for the public API")?
        .context("serving the public API")
}

/// Waits for SIGINT or SIGTERM.
async fn termination() -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt()).context("handling SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("handling SIGTERM")?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}
