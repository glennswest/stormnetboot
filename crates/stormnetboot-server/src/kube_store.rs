//! `BootHost` resources as the host record source.
//!
//! Two halves, both level-triggered. A watch keeps the cluster layer of the
//! host store current, replacing the whole set on every relist so a delete
//! missed during a disconnect heals itself. A status writer walks what we
//! know and patches the objects whose status no longer matches, which means
//! it is safe to restart at any point and safe to run while the apiserver is
//! having a bad day: it converges instead of replaying.
//!
//! Neither half is on the boot path. A cluster that is unreachable costs the
//! fleet its `kubectl get bh` view and nothing else — machines keep booting
//! from the records already in memory, and from the bootstrap file beneath.

use std::{collections::HashMap, sync::Arc, sync::RwLock, time::Duration};

use anyhow::Context as _;
use futures::StreamExt as _;
use kube::{
    Api, Client,
    api::{Patch, PatchParams},
    runtime::watcher,
};

use crate::{
    boothost::{BootHost, BootHostStatus, desired_status},
    http::Shared,
    mac::Mac,
    metrics::Metrics,
};

/// How many status patches one pass may write.
///
/// Ten thousand machines racked at once is the design point, and the first
/// pass after a restart would otherwise be ten thousand writes in a burst
/// against the apiserver that the whole appliance cluster depends on. Capping
/// the pass turns that into a few seconds of convergence, which is the right
/// trade for status nobody is watching in the first second.
const MAX_PATCHES_PER_PASS: usize = 200;

/// A connection to the cluster holding the `BootHost` resources.
#[derive(Clone)]
pub struct KubeLink {
    client: Client,
    /// `None` watches every namespace.
    namespace: Option<String>,
}

impl KubeLink {
    /// Connect the way any in-cluster client connects: the ServiceAccount
    /// token when there is one, `~/.kube/config` when there is not.
    pub async fn connect(namespace: Option<String>) -> anyhow::Result<Self> {
        let client = Client::try_default()
            .await
            .context("connecting to the Kubernetes API")?;
        Ok(Self { client, namespace })
    }

    /// The API to watch: one namespace, or all of them.
    fn watched(&self) -> Api<BootHost> {
        match &self.namespace {
            Some(ns) => Api::namespaced(self.client.clone(), ns),
            None => Api::all(self.client.clone()),
        }
    }

    /// The API to write through. Status patches are namespaced even when the
    /// watch is not, so this is resolved per object rather than once.
    fn in_namespace(&self, namespace: &str) -> Api<BootHost> {
        Api::namespaced(self.client.clone(), namespace)
    }
}

/// The objects the watch has seen, by normalised MAC.
///
/// Kept alongside the host store rather than inside it because the status
/// writer needs the object — its namespace, its generation, and the status it
/// already carries — and the host store deliberately knows only about boot.
#[derive(Debug, Default)]
pub struct BootHostIndex {
    objects: RwLock<HashMap<Mac, BootHost>>,
}

impl BootHostIndex {
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<Mac, BootHost>> {
        self.objects.write().unwrap_or_else(|p| p.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<Mac, BootHost>> {
        self.objects.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Replace everything, from a completed relist.
    fn reset(&self, objects: Vec<BootHost>, state: &Shared) {
        let mut indexed = HashMap::new();
        let mut records = Vec::new();
        for object in objects {
            let Some(record) = object.to_record() else {
                continue;
            };
            records.push(record.clone());
            indexed.insert(record.mac, object);
        }
        *self.write() = indexed;
        state.hosts.set_kube_records(records);
    }

    fn apply(&self, object: BootHost, state: &Shared) {
        let Some(record) = object.to_record() else {
            return;
        };
        self.write().insert(record.mac.clone(), object);
        state.hosts.apply_kube_record(record);
    }

    fn delete(&self, object: &BootHost, state: &Shared) {
        let Some(record) = object.to_record() else {
            return;
        };
        self.write().remove(&record.mac);
        state.hosts.remove_kube_record(&record.mac);
    }

    fn snapshot(&self) -> Vec<(Mac, BootHost)> {
        self.read()
            .iter()
            .map(|(mac, object)| (mac.clone(), object.clone()))
            .collect()
    }

    /// Remember a status we just wrote, so the next pass does not rewrite it
    /// in the window before the watch delivers our own update back to us.
    fn note_status(&self, mac: &Mac, status: BootHostStatus) {
        if let Some(object) = self.write().get_mut(mac) {
            object.status = Some(status);
        }
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }
}

/// Keep the cluster layer of the host store current.
pub fn spawn_watch(state: Shared, index: Arc<BootHostIndex>, link: KubeLink) {
    tokio::spawn(async move {
        // The watcher retries internally with backoff and never ends of its
        // own accord; the outer loop exists only so that a stream which does
        // end is rebuilt instead of leaving the layer frozen.
        loop {
            let mut stream = watcher(link.watched(), watcher::Config::default()).boxed();
            let mut relisting: Vec<BootHost> = Vec::new();

            while let Some(event) = stream.next().await {
                match event {
                    Ok(watcher::Event::Init) => relisting.clear(),
                    Ok(watcher::Event::InitApply(object)) => relisting.push(object),
                    Ok(watcher::Event::InitDone) => {
                        index.reset(std::mem::take(&mut relisting), &state);
                    }
                    Ok(watcher::Event::Apply(object)) => index.apply(object, &state),
                    Ok(watcher::Event::Delete(object)) => index.delete(&object, &state),
                    Err(err) => {
                        Metrics::incr(&state.metrics.watch_errors);
                        // Keep serving the records we have. A boot tier that
                        // stopped answering because the apiserver hiccuped
                        // would be the worse failure by far.
                        tracing::warn!(%err, "BootHost watch error; retrying");
                    }
                }
            }

            tracing::warn!("BootHost watch stream ended; restarting it");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Write back what we have observed about each host.
pub fn spawn_status_writer(
    state: Shared,
    index: Arc<BootHostIndex>,
    link: KubeLink,
    interval: Duration,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            match reconcile_status(&state, &index, &link).await {
                Ok(0) => {}
                Ok(written) => tracing::debug!(written, "BootHost status updated"),
                Err(err) => tracing::warn!(%err, "BootHost status pass failed"),
            }
        }
    });
}

/// One pass. Returns how many objects were patched.
async fn reconcile_status(
    state: &Shared,
    index: &BootHostIndex,
    link: &KubeLink,
) -> anyhow::Result<usize> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let params = PatchParams::default();
    let mut written = 0usize;

    for (mac, object) in index.snapshot() {
        if written >= MAX_PATCHES_PER_PASS {
            break;
        }
        let (Some(namespace), Some(name)) =
            (object.metadata.namespace.clone(), object.metadata.name.clone())
        else {
            continue;
        };

        let desired = desired_status(
            state.boot.get(&mac).as_ref(),
            object.spec.online,
            state.claim_id(&mac).await,
            object.metadata.generation.unwrap_or_default(),
            object.status.as_ref(),
            &now,
        );

        if object.status.as_ref() == Some(&desired) {
            continue;
        }

        let patch = Patch::Merge(serde_json::json!({ "status": desired }));
        match link
            .in_namespace(&namespace)
            .patch_status(&name, &params, &patch)
            .await
        {
            Ok(_) => {
                Metrics::incr(&state.metrics.status_writes);
                index.note_status(&mac, desired);
                written += 1;
            }
            Err(err) => {
                Metrics::incr(&state.metrics.status_write_failures);
                tracing::warn!(%err, object = %format!("{namespace}/{name}"), "status patch failed");
            }
        }
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boothost::BootHostSpec;

    fn boothost(name: &str, mac: &str) -> BootHost {
        let mut bh = BootHost::new(
            name,
            BootHostSpec {
                boot_mac_address: mac.to_owned(),
                hostname: None,
                role: None,
                stack: None,
                portal: None,
                extra_cmdline: None,
                online: true,
            },
        );
        bh.metadata.namespace = Some("storm-system".into());
        bh
    }

    #[test]
    fn the_index_keys_on_the_normalised_mac_whatever_the_yaml_said() {
        let index = BootHostIndex::default();
        let object = boothost("node1", "AA-BB-CC-DD-EE-01");
        index.write().insert(object.to_record().unwrap().mac, object);

        assert!(
            index
                .read()
                .contains_key(&Mac::parse("aa:bb:cc:dd:ee:01").unwrap())
        );
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn a_noted_status_is_what_the_next_pass_compares_against() {
        let index = BootHostIndex::default();
        let object = boothost("node1", "aa:bb:cc:dd:ee:01");
        let mac = object.to_record().unwrap().mac;
        index.write().insert(mac.clone(), object);

        let status = BootHostStatus {
            phase: Some("running".into()),
            ..Default::default()
        };
        index.note_status(&mac, status.clone());

        let (_, seen) = index.snapshot().into_iter().next().unwrap();
        assert_eq!(seen.status, Some(status));
    }

    #[test]
    fn an_object_with_an_unusable_mac_never_enters_the_index() {
        let index = BootHostIndex::default();
        assert!(boothost("broken", "nonsense").to_record().is_none());
        assert_eq!(index.len(), 0);
    }
}
