//! The core's input: a plain-data snapshot of the cluster (data-model.md).
//! Adapters build it from their caches; the core never sees a Kubernetes type.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
    CronJob,
}

impl WorkloadKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::Deployment => "Deployment",
            WorkloadKind::StatefulSet => "StatefulSet",
            WorkloadKind::DaemonSet => "DaemonSet",
            WorkloadKind::CronJob => "CronJob",
        }
    }
}

impl FromStr for WorkloadKind {
    type Err = ParseRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Deployment" => Ok(WorkloadKind::Deployment),
            "StatefulSet" => Ok(WorkloadKind::StatefulSet),
            "DaemonSet" => Ok(WorkloadKind::DaemonSet),
            "CronJob" => Ok(WorkloadKind::CronJob),
            _ => Err(ParseRefError(s.to_owned())),
        }
    }
}

/// A namespaced object name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NsName {
    pub namespace: String,
    pub name: String,
}

impl NsName {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        NsName {
            namespace: namespace.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for NsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

impl FromStr for NsName {
    type Err = ParseRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split('/').collect::<Vec<_>>()[..] {
            [ns, name] if !ns.is_empty() && !name.is_empty() => Ok(NsName::new(ns, name)),
            _ => Err(ParseRefError(s.to_owned())),
        }
    }
}

/// A workload the reaper can observe: `Kind/namespace/name`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkloadRef {
    pub kind: WorkloadKind,
    pub namespace: String,
    pub name: String,
}

impl WorkloadRef {
    pub fn new(kind: WorkloadKind, namespace: impl Into<String>, name: impl Into<String>) -> Self {
        WorkloadRef {
            kind,
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    #[must_use]
    pub fn object_ref(&self) -> ObjectRef {
        ObjectRef {
            kind: match self.kind {
                WorkloadKind::Deployment => ObjectKind::Deployment,
                WorkloadKind::StatefulSet => ObjectKind::StatefulSet,
                WorkloadKind::DaemonSet => ObjectKind::DaemonSet,
                WorkloadKind::CronJob => ObjectKind::CronJob,
            },
            namespace: Some(self.namespace.clone()),
            name: self.name.clone(),
        }
    }
}

impl fmt::Display for WorkloadRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.kind.as_str(), self.namespace, self.name)
    }
}

impl FromStr for WorkloadRef {
    type Err = ParseRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split('/').collect::<Vec<_>>()[..] {
            [kind, ns, name] if !ns.is_empty() && !name.is_empty() => Ok(WorkloadRef::new(
                kind.parse().map_err(|_| ParseRefError(s.to_owned()))?,
                ns,
                name,
            )),
            _ => Err(ParseRefError(s.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid object reference {0:?}")]
pub struct ParseRefError(pub String);

/// Any object the reaper writes to or reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    Deployment,
    StatefulSet,
    DaemonSet,
    CronJob,
    HelmRelease,
    Pod,
    Namespace,
    ReaperConfig,
}

impl ObjectKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Deployment => "Deployment",
            ObjectKind::StatefulSet => "StatefulSet",
            ObjectKind::DaemonSet => "DaemonSet",
            ObjectKind::CronJob => "CronJob",
            ObjectKind::HelmRelease => "HelmRelease",
            ObjectKind::Pod => "Pod",
            ObjectKind::Namespace => "Namespace",
            ObjectKind::ReaperConfig => "ReaperConfig",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    pub namespace: Option<String>,
    pub name: String,
}

impl ObjectRef {
    #[must_use]
    pub fn helm_release(hr: &NsName) -> Self {
        ObjectRef {
            kind: ObjectKind::HelmRelease,
            namespace: Some(hr.namespace.clone()),
            name: hr.name.clone(),
        }
    }
}

impl fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Some(ns) => write!(f, "{}/{}/{}", self.kind.as_str(), ns, self.name),
            None => write!(f, "{}/{}", self.kind.as_str(), self.name),
        }
    }
}

impl Serialize for ObjectRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ObjectRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let kind = |k: &str| {
            [
                ObjectKind::Deployment,
                ObjectKind::StatefulSet,
                ObjectKind::DaemonSet,
                ObjectKind::CronJob,
                ObjectKind::HelmRelease,
                ObjectKind::Pod,
                ObjectKind::Namespace,
                ObjectKind::ReaperConfig,
            ]
            .into_iter()
            .find(|o| o.as_str() == k)
        };
        let parts: Vec<&str> = s.split('/').collect();
        let parsed = match parts[..] {
            [k, ns, name] => kind(k).map(|kind| ObjectRef {
                kind,
                namespace: Some(ns.into()),
                name: name.into(),
            }),
            [k, name] => kind(k).map(|kind| ObjectRef {
                kind,
                namespace: None,
                name: name.into(),
            }),
            _ => None,
        };
        parsed.ok_or_else(|| serde::de::Error::custom(format!("invalid object reference {s:?}")))
    }
}

impl Serialize for NsName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NsName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The cluster as the core sees it at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Injected; the core never reads a clock.
    pub now: Timestamp,
    /// The reaper's own namespace, hard-excluded.
    pub reaper_namespace: String,
    pub namespaces: BTreeMap<String, NamespaceView>,
    pub workloads: Vec<WorkloadView>,
    /// Owner of each ReplicaSet, so pods can be resolved to Deployments.
    pub replica_sets: BTreeMap<NsName, Option<OwnerRef>>,
    pub pods: Vec<PodView>,
    pub helm_releases: BTreeMap<NsName, HelmReleaseView>,
    /// Workloads targeted by a KEDA ScaledObject; empty without KEDA.
    pub keda_targets: BTreeSet<WorkloadRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamespaceView {
    pub name: String,
    /// Raw `reaper.boop.ninja/mode` label and annotation values.
    pub mode_label: Option<String>,
    pub mode_annotation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadView {
    pub workload: WorkloadRef,
    /// For optimistic-concurrency preconditions on writes.
    pub resource_version: String,
    /// `None` for a DaemonSet.
    pub spec_replicas: Option<u32>,
    /// `availableReplicas` (Deployment), `readyReplicas` (StatefulSet) or
    /// `numberAvailable` (DaemonSet).
    pub available_replicas: u32,
    /// The workload itself has an ownerReference with `controller: true`.
    pub has_controller_owner: bool,
    pub has_hpa: bool,
    pub flux: FluxRefs,
    pub mode_label: Option<String>,
    pub mode_annotation: Option<String>,
    /// Only the annotations under `reaper.boop.ninja/`.
    pub reaper_annotations: BTreeMap<String, String>,
    /// The `reaper.boop.ninja/state` label, raw.
    pub state_label: Option<String>,
    /// Schedule state and run history; only for a CronJob.
    pub cron_job: Option<CronJobView>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CronJobView {
    /// `spec.suspend`.
    pub suspended: bool,
    /// `status.lastSuccessfulTime`.
    pub last_successful_time: Option<Timestamp>,
    /// Jobs this CronJob controls (controller ownerReference).
    pub jobs: Vec<JobView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub name: String,
    pub outcome: JobOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Running,
    /// `Complete=True`, finished at `completionTime`.
    Succeeded {
        at: Timestamp,
    },
    /// `Failed=True`, finished when the condition turned true.
    Failed {
        at: Timestamp,
        reason: Option<String>,
    },
}

impl JobView {
    /// When the Job finished; `None` while it runs.
    #[must_use]
    pub fn finished_at(&self) -> Option<Timestamp> {
        match self.outcome {
            JobOutcome::Running => None,
            JobOutcome::Succeeded { at } | JobOutcome::Failed { at, .. } => Some(at),
        }
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.outcome, JobOutcome::Failed { .. })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FluxRefs {
    /// Both `kustomize.toolkit.fluxcd.io/{name,namespace}` labels present.
    pub kustomization: Option<NsName>,
    /// Both `helm.toolkit.fluxcd.io/{name,namespace}` labels present.
    pub helm_release: Option<NsName>,
    /// Current `kustomize.toolkit.fluxcd.io/reconcile` annotation.
    pub reconcile_annotation: Option<String>,
    /// Only one label of a kustomize or helm pair was present.
    pub partial_labels: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRef {
    pub kind: String,
    pub name: String,
    pub controller: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodView {
    pub namespace: String,
    pub name: String,
    /// The controlling ownerReference, if any.
    pub owner: Option<OwnerRef>,
    /// App and init containers.
    pub containers: Vec<ContainerView>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerView {
    pub name: String,
    pub init: bool,
    pub ready: bool,
    pub restart_count: u32,
    pub waiting_reason: Option<String>,
    /// Current `state.terminated`.
    pub terminated: Option<Termination>,
    /// `lastState.terminated`.
    pub last_terminated: Option<Termination>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Termination {
    pub exit_code: i32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelmReleaseView {
    pub helm_release: NsName,
    pub resource_version: String,
    pub suspended: bool,
    /// The HelmRelease's own kustomize labels.
    pub kustomization: Option<NsName>,
    pub reconcile_annotation: Option<String>,
    /// Only the annotations under `reaper.boop.ninja/`.
    pub reaper_annotations: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_round_trip() {
        let w: WorkloadRef = "Deployment/app/web".parse().unwrap();
        assert_eq!(w, WorkloadRef::new(WorkloadKind::Deployment, "app", "web"));
        assert_eq!(w.to_string(), "Deployment/app/web");
        assert!("Job/app/web".parse::<WorkloadRef>().is_err());
        let c: WorkloadRef = "CronJob/app/nightly".parse().unwrap();
        assert_eq!(c.object_ref().to_string(), "CronJob/app/nightly");
        assert!("Deployment//web".parse::<WorkloadRef>().is_err());
        assert_eq!("ns/hr".parse::<NsName>().unwrap().to_string(), "ns/hr");
        assert!("ns/hr/x".parse::<NsName>().is_err());
        let o = ObjectRef::helm_release(&NsName::new("helm", "web"));
        let j = serde_json::to_string(&o).unwrap();
        assert_eq!(j, "\"HelmRelease/helm/web\"");
        assert_eq!(serde_json::from_str::<ObjectRef>(&j).unwrap(), o);
    }
}
