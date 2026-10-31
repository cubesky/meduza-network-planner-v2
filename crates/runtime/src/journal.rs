//! Ownership and apply journals (§9.6, §11.1, §11.3).
//!
//! Ownership is recorded *before* a resource is created ("intent"), then promoted to "committed" once the
//! apply step succeeded. After a crash, both sets are known, so planner can discover and reclaim resources it
//! may have created without guessing from familiar names.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use meduza_np_protocol::plan::NetworkResource;
use serde::{Deserialize, Serialize};

use crate::fsutil;

pub const OWNERSHIP_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedSet {
    #[serde(default)]
    pub resources: Vec<NetworkResource>,
    #[serde(default)]
    pub processes: BTreeSet<String>,
}

impl OwnedSet {
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.processes.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceOwnership {
    /// What the last successful apply left in place.
    #[serde(default)]
    pub committed: OwnedSet,
    /// What an in-flight apply may have created; non-empty after a crash.
    #[serde(default)]
    pub intent: OwnedSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipJournal {
    pub version: u32,
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceOwnership>,
    #[serde(skip)]
    path: PathBuf,
}

impl OwnershipJournal {
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut j: OwnershipJournal = fsutil::read_json(path, MAX_JOURNAL_BYTES)?.unwrap_or(OwnershipJournal {
            version: OWNERSHIP_VERSION,
            instances: BTreeMap::new(),
            path: PathBuf::new(),
        });
        if j.version != OWNERSHIP_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported ownership journal version {}", j.version)));
        }
        j.path = path.to_owned();
        Ok(j)
    }

    fn save(&self) -> io::Result<()> {
        fsutil::write_json_atomic(&self.path, self, 0o600)
    }

    /// Records that `instance` is about to create/own `set` (merged into any previous intent). Persisted first.
    pub fn record_intent(&mut self, instance: &str, set: &OwnedSet) -> io::Result<()> {
        let entry = self.instances.entry(instance.to_owned()).or_default();
        for r in &set.resources {
            if !entry.intent.resources.contains(r) {
                entry.intent.resources.push(r.clone());
            }
        }
        entry.intent.processes.extend(set.processes.iter().cloned());
        self.save()
    }

    /// The apply of `instance` succeeded with exactly `set` in place.
    pub fn commit(&mut self, instance: &str, set: OwnedSet) -> io::Result<()> {
        if set.is_empty() {
            self.instances.remove(instance);
        } else {
            let entry = self.instances.entry(instance.to_owned()).or_default();
            entry.committed = set;
            entry.intent = OwnedSet::default();
        }
        self.save()
    }

    /// Everything planner may own for `instance`: committed ∪ intent. Used for cleanup and crash recovery.
    pub fn possibly_owned(&self, instance: &str) -> OwnedSet {
        let Some(entry) = self.instances.get(instance) else { return OwnedSet::default() };
        let mut set = entry.committed.clone();
        for r in &entry.intent.resources {
            if !set.resources.contains(r) {
                set.resources.push(r.clone());
            }
        }
        set.processes.extend(entry.intent.processes.iter().cloned());
        set
    }

    /// Instances with an unfinished intent (crash recovery candidates).
    pub fn dirty_instances(&self) -> Vec<String> {
        self.instances.iter().filter(|(_, o)| !o.intent.is_empty()).map(|(k, _)| k.clone()).collect()
    }

    pub fn instances(&self) -> impl Iterator<Item = &String> {
        self.instances.keys()
    }

    /// Returns true if any instance other than `instance` owns an equal resource (conflict check).
    pub fn owner_of(&self, resource: &NetworkResource) -> Option<&str> {
        self.instances
            .iter()
            .find(|(_, o)| o.committed.resources.contains(resource) || o.intent.resources.contains(resource))
            .map(|(k, _)| k.as_str())
    }
}

/// Resources present in `old` but not in `new` (to remove) and in `new` but not `old` (to add).
pub fn diff_resources(old: &[NetworkResource], new: &[NetworkResource]) -> (Vec<NetworkResource>, Vec<NetworkResource>) {
    let remove = old.iter().filter(|r| !new.contains(r)).cloned().collect();
    let add = new.iter().filter(|r| !old.contains(r)).cloned().collect();
    (remove, add)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyStage {
    Validating,
    Planned,
    Staged,
    Applying,
    Probing,
    Committed,
    RollingBack,
    Failed,
}

/// Persisted before touching the system so a restart knows an apply was interrupted (§11.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApply {
    pub revision: String,
    pub config_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_revision: Option<String>,
    pub stage: ApplyStage,
    pub started_at: String,
    #[serde(default)]
    pub completed_instances: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PendingApply {
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        fsutil::read_json(path, MAX_JOURNAL_BYTES)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        fsutil::write_json_atomic(path, self, 0o600)
    }

    pub fn clear(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(dev: &str, cidr: &str) -> NetworkResource {
        NetworkResource::Address { device: dev.into(), cidr: cidr.into() }
    }

    #[test]
    fn intent_then_commit_survives_reload() {
        let dir = std::env::temp_dir().join(format!("mnp-journal-{}", std::process::id()));
        let path = dir.join("ownership.json");
        let mut j = OwnershipJournal::load(&path).unwrap();
        let set = OwnedSet { resources: vec![addr("wg0", "10.0.0.1/24")], processes: ["wireguard/wg0".to_string()].into() };
        j.record_intent("wireguard/wg0", &set).unwrap();
        // Simulated crash: reload sees the dirty intent.
        let j2 = OwnershipJournal::load(&path).unwrap();
        assert_eq!(j2.dirty_instances(), vec!["wireguard/wg0".to_string()]);
        assert_eq!(j2.possibly_owned("wireguard/wg0"), set);
        assert_eq!(j2.owner_of(&addr("wg0", "10.0.0.1/24")), Some("wireguard/wg0"));
        let mut j3 = j2;
        j3.commit("wireguard/wg0", set.clone()).unwrap();
        let j4 = OwnershipJournal::load(&path).unwrap();
        assert!(j4.dirty_instances().is_empty());
        assert_eq!(j4.instances["wireguard/wg0"].committed, set);
        let mut j5 = j4;
        j5.commit("wireguard/wg0", OwnedSet::default()).unwrap();
        assert!(OwnershipJournal::load(&path).unwrap().instances.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resource_diff() {
        let old = vec![addr("a", "1"), addr("b", "2")];
        let new = vec![addr("b", "2"), addr("c", "3")];
        let (remove, add) = diff_resources(&old, &new);
        assert_eq!(remove, vec![addr("a", "1")]);
        assert_eq!(add, vec![addr("c", "3")]);
    }
}
