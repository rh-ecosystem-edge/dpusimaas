use std::{fmt, net::Ipv4Addr};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// How DPF provisions the simulated DPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// DPF provisions the DPU through its BMC (Redfish); the host is only reset over IPMI.
    ZeroTrust,
    /// DPF provisions the DPU through its host; something on the host side (the sim's hostagent)
    /// hands dpusimaas the bf.cfg to boot.
    HostTrusted,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::ZeroTrust, Mode::HostTrusted];

    pub fn name(self) -> &'static str {
        match self {
            Mode::ZeroTrust => "zero-trust",
            Mode::HostTrusted => "host-trusted",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// How much of the DPU is real.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// DPF installs the DPU through its (mock) BMC, then a real aarch64 VM boots DPF's OS from that
    /// install and joins the DPU cluster. The host is only its BMC's power switch.
    Vm,
}

impl Level {
    pub const ALL: [Level; 1] = [Level::Vm];

    pub fn name(self) -> &'static str {
        match self {
            Level::Vm => "vm",
        }
    }

    /// What the level simulates, for clients to show.
    pub fn description(self) -> &'static str {
        match self {
            Level::Vm => {
                "Real DPU VM: DPF installs it and its OS runs in an aarch64 VM; the host is only a power switch"
            }
        }
    }
}

/// What a lease is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaseState {
    /// Getting an address.
    Starting,
    /// BMCs up, waiting for DPF to install an OS.
    Waiting,
    /// DPU VM being created from the installed bf.cfg.
    Booting,
    /// DPU VM up.
    Running,
    /// Host powered off over IPMI.
    PoweredOff,
    Failed,
}

/// One simulated DPU host: an address on the lab LAN (from DHCP) where its BMCs answer, and the
/// DPU VM that boots once DPF installs an OS on it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub owner: String,
    pub mode: Mode,
    #[serde(default = "default_level", deserialize_with = "level_or_vm")]
    pub level: Level,
    pub created: DateTime<Utc>,
    /// Only set when the lease asked for an end; otherwise it lasts until released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,

    /// The lease's own MAC on the bridge; its address comes from the lab DHCP server for that MAC.
    pub mac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<Ipv4Addr>,

    /// What DPF needs to talk to it. Both BMCs answer on `ip`: the DPU's BMC (Redfish, HTTPS 443)
    /// and the host's BMC (IPMI lanplus, 623).
    pub serial: String,
    pub bmc_password: String,
    pub ipmi_user: String,
    pub ipmi_password: String,

    /// The DPU VM (dpusim names its NICs after the lease) and what the lease is doing.
    pub vm: String,
    pub state: LeaseState,

    /// Host-trusted: the host the cluster installs, a worker like any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<Host>,

    #[serde(default, deserialize_with = "null_as_empty")]
    pub events: Vec<LeaseEvent>,
}

/// A host-trusted lease's host, as the cluster adds it as a worker: its BMC is on the lease's
/// address (Redfish with virtual media on 443, IPMI lanplus on 623, both as `ipmi_user`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    /// The host VM.
    pub vm: String,
    /// The NIC it's installed and joins the cluster through.
    pub boot_mac: String,
    pub root_device: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseEvent {
    pub time: DateTime<Utc>,
    pub message: String,
}

/// How many events a lease keeps, newest last.
const MAX_EVENTS: usize = 200;

/// What a new lease is for, once its request is validated.
pub struct LeaseTerms {
    pub owner: String,
    pub mode: Mode,
    pub level: Level,
    pub bmc_password: Option<String>,
    pub expires: Option<DateTime<Utc>>,
}

impl Lease {
    pub fn new(terms: LeaseTerms) -> Lease {
        let id = random_hex_digits(5);
        let mut lease = Lease {
            mode: terms.mode,
            level: terms.level,
            created: Utc::now(),
            expires: terms.expires,
            mac: derive_mac(&id, "bmc"),
            ip: None,
            serial: serial_for(&id),
            bmc_password: terms.bmc_password.unwrap_or_else(random_password),
            ipmi_user: "admin".to_owned(),
            ipmi_password: random_password(),
            vm: format!("dpusimaas-{id}"),
            state: LeaseState::Starting,
            host: None,
            events: Vec::new(),
            owner: terms.owner,
            id,
        };
        lease.record(format!(
            "leased to {} ({}){}",
            lease.owner,
            lease.mode,
            until_text(lease.expires)
        ));
        lease
    }

    pub fn record(&mut self, message: impl Into<String>) {
        self.events.push(LeaseEvent {
            time: Utc::now(),
            message: message.into(),
        });
        let excess = self.events.len().saturating_sub(MAX_EVENTS);
        self.events.drain(..excess);
    }

    /// The host VM of a host-trusted lease.
    pub fn host_vm(&self) -> String {
        format!("{}-host", self.vm)
    }

    pub fn has_expired(&self) -> bool {
        self.expires.is_some_and(|expires| Utc::now() > expires)
    }
}

/// The end of a lease that asked for one (`ttl_hours` > 0); none otherwise.
pub fn expiry(ttl_hours: i64) -> Option<DateTime<Utc>> {
    if ttl_hours <= 0 {
        return None;
    }
    TimeDelta::try_hours(ttl_hours).and_then(|ttl| Utc::now().checked_add_signed(ttl))
}

pub fn until_text(expires: Option<DateTime<Utc>>) -> String {
    expires
        .map(|expires| {
            format!(
                " until {}",
                expires.to_rfc3339_opts(SecondsFormat::Secs, true)
            )
        })
        .unwrap_or_default()
}

fn random_hex_digits(count: usize) -> String {
    (0..count)
        .filter_map(|_| char::from_digit(rand::random_range(0..16), 16))
        .collect()
}

/// A random password that fits IPMI's 20-byte limit.
fn random_password() -> String {
    format!("Sim{}", random_hex_digits(13))
}

/// A stable, locally administered unicast MAC for one role of a lease.
fn derive_mac(lease_id: &str, role: &str) -> String {
    let digest = Sha256::digest(format!("dpusimaas/{lease_id}/{role}"));
    let prefix = [0x52_u8, 0x5a];
    prefix
        .iter()
        .chain(digest.iter().take(4))
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A BlueField serial for the lease, valid for the mock BMC (MT, year, week, six upper-case
/// characters) and unique per lease.
fn serial_for(lease_id: &str) -> String {
    format!("MT2643S{}", lease_id.to_uppercase())
}

fn default_level() -> Level {
    Level::Vm
}

/// Leases from before levels have an empty one: they're VM leases.
fn level_or_vm<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Level, D::Error> {
    match String::deserialize(deserializer)?.as_str() {
        "" | "vm" => Ok(Level::Vm),
        unknown => Err(serde::de::Error::unknown_variant(unknown, &["vm"])),
    }
}

/// An empty list is `null` in leases dpusimaas wrote while it was in Go.
fn null_as_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<LeaseEvent>, D::Error> {
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_mac_is_stable_and_locally_administered() {
        let mac = derive_mac("abcde", "bmc");
        assert_eq!(mac, derive_mac("abcde", "bmc"));
        assert_ne!(mac, derive_mac("abcdf", "bmc"));
        assert!(mac.starts_with("52:5a:"), "{mac}");
        assert_eq!(mac.split(':').count(), 6, "{mac}");
    }

    #[test]
    fn password_fits_ipmi() {
        let password = random_password();
        assert_eq!(password.len(), 16, "{password}");
        assert!(password.starts_with("Sim"), "{password}");
    }

    #[test]
    fn events_are_capped() {
        let mut lease = Lease::new(LeaseTerms {
            owner: "someone".to_owned(),
            mode: Mode::ZeroTrust,
            level: Level::Vm,
            bmc_password: None,
            expires: None,
        });
        (0..250).for_each(|event_number| lease.record(format!("event {event_number}")));
        assert_eq!(lease.events.len(), MAX_EVENTS);
        assert_eq!(
            lease.events.last().map(|event| event.message.as_str()),
            Some("event 249")
        );
    }

    #[test]
    fn reads_a_lease_the_go_dpusimaas_wrote() -> anyhow::Result<()> {
        let stored = r#"{
          "id": "a1b2c", "owner": "someone", "mode": "zero-trust", "level": "",
          "created": "2026-10-20T08:15:30.123456789Z", "mac": "52:5a:01:02:03:04",
          "ip": "10.0.0.7", "serial": "MT2643SA1B2C", "bmc_password": "pw",
          "ipmi_user": "admin", "ipmi_password": "pw2", "vm": "dpusimaas-a1b2c",
          "state": "powered-off", "events": null
        }"#;
        let lease: Lease = serde_json::from_str(stored)?;
        assert_eq!(lease.level, Level::Vm);
        assert_eq!(lease.state, LeaseState::PoweredOff);
        assert_eq!(lease.ip, Some(Ipv4Addr::new(10, 0, 0, 7)));
        assert!(lease.events.is_empty());
        assert!(lease.expires.is_none());
        Ok(())
    }
}
