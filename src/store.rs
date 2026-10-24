use std::{
    collections::BTreeMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use parking_lot::Mutex;

use crate::lease::Lease;

/// The leases, kept in a JSON file so a restart picks them up again.
pub struct LeaseStore {
    path: PathBuf,
    leases: Mutex<BTreeMap<String, Lease>>,
}

impl LeaseStore {
    pub fn open(state_dir: &Path) -> Result<LeaseStore> {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let path = state_dir.join("leases.json");
        let leases = match fs::read(&path) {
            Ok(stored) => serde_json::from_slice::<Vec<Lease>>(&stored)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let leases_by_id = leases
            .into_iter()
            .map(|lease| (lease.id.clone(), lease))
            .collect();
        Ok(LeaseStore {
            path,
            leases: Mutex::new(leases_by_id),
        })
    }

    pub fn add(&self, lease: Lease) -> Result<()> {
        let mut leases = self.leases.lock();
        leases.insert(lease.id.clone(), lease);
        self.save(&leases)
    }

    /// Changes a lease and saves; returns the changed lease, or none when there's no such lease.
    pub fn update(&self, lease_id: &str, change: impl FnOnce(&mut Lease)) -> Result<Option<Lease>> {
        let mut leases = self.leases.lock();
        let Some(lease) = leases.get_mut(lease_id) else {
            return Ok(None);
        };
        change(lease);
        let changed = lease.clone();
        self.save(&leases)?;
        Ok(Some(changed))
    }

    pub fn remove(&self, lease_id: &str) -> Result<()> {
        let mut leases = self.leases.lock();
        leases.remove(lease_id);
        self.save(&leases)
    }

    pub fn get(&self, lease_id: &str) -> Option<Lease> {
        self.leases.lock().get(lease_id).cloned()
    }

    /// The leases, oldest first.
    pub fn list(&self) -> Vec<Lease> {
        let mut leases: Vec<Lease> = self.leases.lock().values().cloned().collect();
        leases.sort_by_key(|lease| lease.created);
        leases
    }

    pub fn count(&self) -> usize {
        self.leases.lock().len()
    }

    fn save(&self, leases: &BTreeMap<String, Lease>) -> Result<()> {
        let mut oldest_first: Vec<&Lease> = leases.values().collect();
        oldest_first.sort_by_key(|lease| lease.created);
        let serialized = serde_json::to_vec_pretty(&oldest_first).context("serializing leases")?;
        let staged = self.path.with_extension("json.tmp");
        write_private(&staged, &serialized)
            .with_context(|| format!("writing {}", staged.display()))?;
        fs::rename(&staged, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))
    }
}

/// Writes a file only root can read: it holds the leases' BMC passwords.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{Level, Mode};

    fn some_lease(owner: &str) -> Lease {
        Lease::new(crate::lease::LeaseTerms {
            owner: owner.to_owned(),
            cluster: crate::lease::default_cluster(),
            mode: Mode::HostTrusted,
            level: Level::Vm,
            bmc_password: None,
            expires: None,
        })
    }

    #[test]
    fn leases_survive_reopening() -> Result<()> {
        let state_dir =
            std::env::temp_dir().join(format!("dpusimaas-store-{}", std::process::id()));
        let store = LeaseStore::open(&state_dir)?;
        let first = some_lease("first");
        let second = some_lease("second");
        store.add(first.clone())?;
        store.add(second.clone())?;
        store.update(&first.id, |lease| lease.record("changed"))?;
        store.remove(&second.id)?;

        let reopened = LeaseStore::open(&state_dir)?;
        let restored = reopened.get(&first.id).context("the first lease is gone")?;
        assert_eq!(restored.events.len(), 2);
        assert!(reopened.get(&second.id).is_none());
        fs::remove_dir_all(&state_dir).with_context(|| format!("removing {}", state_dir.display()))
    }
}
