//! Host records — who a booting machine is, and what it should boot.
//!
//! Identity is pinned here at PXE time, which is what makes day 2 a profile
//! change rather than a reprovision: the name and role a node carries for the
//! rest of its life are decided before it has an OS to decide them with.
//!
//! Records come from a file today. The intended source is a rustkube resource
//! (see the BootHost CRD in `deploy/manifests`), and the split in this module
//! is drawn so that swapping the backing store does not touch callers.

use std::{collections::HashMap, path::PathBuf, sync::RwLock, time::SystemTime};

use serde::{Deserialize, Serialize};

use crate::mac::Mac;

/// What a specific machine should boot, and as whom.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostRecord {
    pub mac: Mac,
    /// Hostname this machine takes. Pinned now; used for the rest of its life.
    pub name: String,
    /// Role the node will play once it joins — the day-2 profile to apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Stack (loadout) to boot, e.g. `stormcos:10.20`. Falls back to the
    /// server default when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// NVMe/TCP portal override, for a host that should attach to a nearer
    /// appliance than the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal: Option<String>,
    /// Extra kernel command line for this host alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_cmdline: Option<String>,
}

/// What to do when a machine we have no record for asks to boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum UnknownHostPolicy {
    /// Serve it the defaults. This is how a fleet is discovered: on a
    /// provisioning network every machine is unknown the first time.
    Allow,
    /// Refuse. For sites where the boot network is not trusted to contain
    /// only machines that belong there.
    Deny,
}

impl std::str::FromStr for UnknownHostPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            other => Err(format!("expected 'allow' or 'deny', found {other:?}")),
        }
    }
}

#[derive(Debug, Default)]
struct Loaded {
    records: HashMap<Mac, HostRecord>,
    /// Modification time of the file these came from, so a reload is a no-op
    /// when nothing changed.
    mtime: Option<SystemTime>,
}

/// Host record store.
#[derive(Debug)]
pub struct HostStore {
    path: Option<PathBuf>,
    loaded: RwLock<Loaded>,
}

impl HostStore {
    /// A store with no records — every host is unknown.
    pub fn empty() -> Self {
        Self {
            path: None,
            loaded: RwLock::new(Loaded::default()),
        }
    }

    /// A store backed by a JSON file holding an array of [`HostRecord`].
    ///
    /// A missing file is not an error: a site may run entirely on defaults,
    /// and the file may appear later.
    pub fn from_file(path: PathBuf) -> Self {
        let store = Self {
            path: Some(path),
            loaded: RwLock::new(Loaded::default()),
        };
        if let Err(err) = store.reload() {
            tracing::warn!(%err, "could not load host records at startup");
        }
        store
    }

    /// Re-read the backing file if it changed. Returns the number of records.
    pub fn reload(&self) -> anyhow::Result<usize> {
        let Some(path) = &self.path else {
            return Ok(0);
        };

        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        {
            let loaded = self.read();
            if mtime.is_some() && loaded.mtime == mtime {
                return Ok(loaded.records.len());
            }
        }

        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no host record file yet");
                return Ok(0);
            }
            Err(err) => return Err(err.into()),
        };

        let list: Vec<HostRecord> = serde_json::from_str(&raw)?;
        let count = list.len();
        let records = list.into_iter().map(|r| (r.mac.clone(), r)).collect();

        let mut loaded = self.write();
        loaded.records = records;
        loaded.mtime = mtime;
        tracing::info!(count, path = %path.display(), "loaded host records");
        Ok(count)
    }

    pub fn lookup(&self, mac: &Mac) -> Option<HostRecord> {
        self.read().records.get(mac).cloned()
    }

    pub fn len(&self) -> usize {
        self.read().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Loaded> {
        self.loaded.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Loaded> {
        self.loaded.write().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_file(contents: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "snb-hosts-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn loads_records_and_looks_them_up_by_any_mac_spelling() {
        let path = temp_file(
            r#"[
                {"mac":"AA-BB-CC-DD-EE-01","name":"node1","role":"master"},
                {"mac":"aabb.ccdd.ee02","name":"node2","stack":"stormcos:10.20"}
            ]"#,
        );
        let store = HostStore::from_file(path.clone());
        assert_eq!(store.len(), 2);

        // Stored from dashes, looked up with colons: same host.
        let found = store.lookup(&Mac::parse("aa:bb:cc:dd:ee:01").unwrap()).unwrap();
        assert_eq!(found.name, "node1");
        assert_eq!(found.role.as_deref(), Some("master"));

        let found2 = store.lookup(&Mac::parse("aa:bb:cc:dd:ee:02").unwrap()).unwrap();
        assert_eq!(found2.stack.as_deref(), Some("stormcos:10.20"));

        assert!(store.lookup(&Mac::parse("00:00:00:00:00:00").unwrap()).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let store = HostStore::from_file(PathBuf::from("/nonexistent/hosts.json"));
        assert!(store.is_empty());
        assert_eq!(store.reload().unwrap(), 0);
    }

    #[test]
    fn empty_store_knows_nobody() {
        let store = HostStore::empty();
        assert!(store.is_empty());
        assert!(store.lookup(&Mac::parse("aa:bb:cc:dd:ee:ff").unwrap()).is_none());
    }

    #[test]
    fn policy_parses_both_ways() {
        use std::str::FromStr as _;
        assert_eq!(UnknownHostPolicy::from_str("allow"), Ok(UnknownHostPolicy::Allow));
        assert_eq!(UnknownHostPolicy::from_str("DENY"), Ok(UnknownHostPolicy::Deny));
        assert!(UnknownHostPolicy::from_str("maybe").is_err());
    }

    #[test]
    fn malformed_json_surfaces_as_an_error() {
        let path = temp_file("{not an array}");
        let store = HostStore {
            path: Some(path.clone()),
            loaded: RwLock::new(Loaded::default()),
        };
        assert!(store.reload().is_err());
        let _ = std::fs::remove_file(path);
    }
}
