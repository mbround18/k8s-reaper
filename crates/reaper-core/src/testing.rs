//! Fixture builders and in-memory port fakes, for core tests and adapter
//! contract tests. Not used by the running reaper.

use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, ready};
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use crate::config::Mode;
use crate::model::{
    ContainerView, FluxRefs, HelmReleaseView, NamespaceView, NsName, ObjectRef, OwnerRef, PodView,
    Snapshot, Termination, WorkloadKind, WorkloadRef, WorkloadView,
};
use crate::ports::{
    ActionKind, ActionResult, ClusterWriter, EventRecord, EventSink, HelmReleasePatch, Telemetry,
    WorkloadRow, WriteError,
};
use crate::record::MetadataPatch;
use crate::time::Timestamp;

/// Drive a future that never actually waits (the fakes are all ready at
/// once) without pulling an async runtime into the core.
///
/// # Panics
/// If the future is not ready after a bounded number of polls.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..10_000 {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
    }
    panic!("block_on: future did not complete; fakes must never wait");
}

/// `2026-10-10T12:00:00Z`, the default "now" in fixtures.
pub const NOW: Timestamp = Timestamp(1_791_633_600);

#[must_use]
pub fn ts(rfc3339: &str) -> Timestamp {
    Timestamp::parse_rfc3339(rfc3339).expect("valid fixture timestamp")
}

/// `NOW` minus `minutes`.
#[must_use]
pub fn mins_ago(minutes: i64) -> Timestamp {
    Timestamp(NOW.0 - minutes * 60)
}

#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        now: NOW,
        reaper_namespace: "k8s-reaper".into(),
        namespaces: BTreeMap::new(),
        workloads: Vec::new(),
        replica_sets: BTreeMap::new(),
        pods: Vec::new(),
        helm_releases: BTreeMap::new(),
        keda_targets: BTreeSet::new(),
    }
}

/// A workload with sensible defaults: 2 desired replicas, 0 available, no
/// Flux, no reaper state.
#[must_use]
pub fn workload(kind: WorkloadKind, namespace: &str, name: &str) -> WorkloadView {
    let replicas = match kind {
        WorkloadKind::Deployment | WorkloadKind::StatefulSet => Some(2),
        WorkloadKind::DaemonSet | WorkloadKind::CronJob => None,
    };
    WorkloadView {
        workload: WorkloadRef::new(kind, namespace, name),
        resource_version: "1".into(),
        spec_replicas: replicas,
        available_replicas: 0,
        has_controller_owner: false,
        has_hpa: false,
        flux: FluxRefs::default(),
        mode_label: None,
        mode_annotation: None,
        reaper_annotations: BTreeMap::new(),
        state_label: None,
    }
}

#[must_use]
pub fn deployment(namespace: &str, name: &str) -> WorkloadView {
    workload(WorkloadKind::Deployment, namespace, name)
}

/// A container in `CrashLoopBackOff` after exiting 1.
#[must_use]
pub fn crashlooping_container(name: &str, restarts: u32) -> ContainerView {
    ContainerView {
        name: name.into(),
        init: false,
        ready: false,
        restart_count: restarts,
        waiting_reason: Some("CrashLoopBackOff".into()),
        terminated: None,
        last_terminated: Some(Termination {
            exit_code: 1,
            reason: "Error".into(),
        }),
    }
}

#[must_use]
pub fn healthy_container(name: &str) -> ContainerView {
    ContainerView {
        name: name.into(),
        ready: true,
        ..ContainerView::default()
    }
}

/// A pod owned the way the real controller would own it. Deployment pods get
/// a ReplicaSet `<name>-rs` registered in the snapshot.
pub fn add_pod(
    snap: &mut Snapshot,
    owner: &WorkloadRef,
    pod: &str,
    containers: Vec<ContainerView>,
) {
    let owner_ref = match owner.kind {
        WorkloadKind::Deployment => {
            let rs = format!("{}-rs", owner.name);
            snap.replica_sets.insert(
                NsName::new(&owner.namespace, &rs),
                Some(OwnerRef {
                    kind: "Deployment".into(),
                    name: owner.name.clone(),
                    controller: true,
                }),
            );
            OwnerRef {
                kind: "ReplicaSet".into(),
                name: rs,
                controller: true,
            }
        }
        WorkloadKind::StatefulSet | WorkloadKind::DaemonSet => OwnerRef {
            kind: owner.kind.as_str().into(),
            name: owner.name.clone(),
            controller: true,
        },
        WorkloadKind::CronJob => OwnerRef {
            kind: "Job".into(),
            name: format!("{}-job", owner.name),
            controller: true,
        },
    };
    snap.pods.push(PodView {
        namespace: owner.namespace.clone(),
        name: pod.into(),
        owner: Some(owner_ref),
        containers,
    });
}

pub fn add_namespace(snap: &mut Snapshot, name: &str, mode_label: Option<&str>) {
    snap.namespaces.insert(
        name.into(),
        NamespaceView {
            name: name.into(),
            mode_label: mode_label.map(Into::into),
            mode_annotation: None,
        },
    );
}

#[must_use]
pub fn helm_release(namespace: &str, name: &str) -> HelmReleaseView {
    HelmReleaseView {
        helm_release: NsName::new(namespace, name),
        resource_version: "1".into(),
        suspended: false,
        kustomization: None,
        reconcile_annotation: None,
        reaper_annotations: BTreeMap::new(),
    }
}

/// One write as the fake writer saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    PatchMetadata {
        target: ObjectRef,
        resource_version: String,
        patch: MetadataPatch,
    },
    Scale {
        workload: WorkloadRef,
        resource_version: String,
        replicas: u32,
    },
    SetCronJobSuspend {
        workload: WorkloadRef,
        resource_version: String,
        suspend: bool,
    },
    PatchHelmRelease {
        helm_release: NsName,
        resource_version: String,
        patch: HelmReleasePatch,
    },
}

impl Call {
    /// True for a write that only touches `reaper.boop.ninja/*` metadata.
    #[must_use]
    pub fn only_reaper_metadata(&self) -> bool {
        match self {
            Call::PatchMetadata { patch, .. } => patch.only_reaper_keys(),
            Call::PatchHelmRelease { patch, .. } => {
                patch.suspend.is_none()
                    && patch
                        .annotations
                        .keys()
                        .all(|k| k.starts_with(crate::record::PREFIX))
            }
            Call::Scale { .. } | Call::SetCronJobSuspend { .. } => false,
        }
    }
}

type FailWhen = Box<dyn Fn(&Call) -> Option<WriteError> + Send>;

/// Records every write and hands out increasing resourceVersions.
#[derive(Default)]
pub struct FakeWriter {
    pub calls: Mutex<Vec<Call>>,
    next_rv: Mutex<u64>,
    fail_when: Mutex<Option<FailWhen>>,
}

impl FakeWriter {
    #[must_use]
    pub fn new() -> Self {
        FakeWriter {
            next_rv: Mutex::new(100),
            ..FakeWriter::default()
        }
    }

    /// Fail every call for which `f` returns an error.
    pub fn fail_when(&self, f: impl Fn(&Call) -> Option<WriteError> + Send + 'static) {
        *self.fail_when.lock().unwrap() = Some(Box::new(f));
    }

    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: Call) -> impl Future<Output = Result<String, WriteError>> + Send {
        let failure = self
            .fail_when
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|f| f(&call));
        self.calls.lock().unwrap().push(call);
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let mut rv = self.next_rv.lock().unwrap();
        *rv += 1;
        ready(Ok(rv.to_string()))
    }
}

impl ClusterWriter for FakeWriter {
    fn patch_metadata(
        &self,
        target: &ObjectRef,
        resource_version: &str,
        patch: &MetadataPatch,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        self.record(Call::PatchMetadata {
            target: target.clone(),
            resource_version: resource_version.into(),
            patch: patch.clone(),
        })
    }

    fn scale(
        &self,
        workload: &WorkloadRef,
        resource_version: &str,
        replicas: u32,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        self.record(Call::Scale {
            workload: workload.clone(),
            resource_version: resource_version.into(),
            replicas,
        })
    }

    fn set_cronjob_suspend(
        &self,
        workload: &WorkloadRef,
        resource_version: &str,
        suspend: bool,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        self.record(Call::SetCronJobSuspend {
            workload: workload.clone(),
            resource_version: resource_version.into(),
            suspend,
        })
    }

    fn patch_helm_release(
        &self,
        helm_release: &NsName,
        resource_version: &str,
        patch: &HelmReleasePatch,
    ) -> impl Future<Output = Result<String, WriteError>> + Send {
        self.record(Call::PatchHelmRelease {
            helm_release: helm_release.clone(),
            resource_version: resource_version.into(),
            patch: patch.clone(),
        })
    }
}

#[derive(Default)]
pub struct FakeEvents {
    pub events: Mutex<Vec<EventRecord>>,
}

impl FakeEvents {
    #[must_use]
    pub fn reasons(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.reason)
            .collect()
    }
}

impl EventSink for FakeEvents {
    fn publish(&self, event: &EventRecord) -> impl Future<Output = Result<(), WriteError>> + Send {
        self.events.lock().unwrap().push(event.clone());
        ready(Ok(()))
    }
}

#[derive(Default)]
pub struct FakeTelemetry {
    pub rows: Mutex<Vec<WorkloadRow>>,
    pub actions: Mutex<Vec<(ActionKind, ActionResult, Mode)>>,
}

impl Telemetry for FakeTelemetry {
    fn workloads(&self, rows: &[WorkloadRow]) {
        *self.rows.lock().unwrap() = rows.to_vec();
    }

    fn action(&self, kind: ActionKind, result: ActionResult, mode: Mode) {
        self.actions.lock().unwrap().push((kind, result, mode));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_the_documented_instant() {
        assert_eq!(NOW, ts("2026-10-10T12:00:00Z"));
        assert_eq!(mins_ago(30).to_string(), "2026-10-10T11:30:00Z");
    }

    #[test]
    fn deployment_pods_get_a_replicaset_chain() {
        let mut snap = snapshot();
        let d = deployment("app", "web");
        add_pod(
            &mut snap,
            &d.workload,
            "web-rs-1",
            vec![crashlooping_container("api", 12)],
        );
        let owner = snap.pods[0].owner.clone().unwrap();
        assert_eq!(owner.kind, "ReplicaSet");
        let rs_owner = snap.replica_sets[&NsName::new("app", &owner.name)]
            .clone()
            .unwrap();
        assert_eq!(rs_owner.name, "web");
    }

    #[test]
    fn fake_writer_threads_resource_versions_and_failures() {
        let w = FakeWriter::new();
        let d = WorkloadRef::new(WorkloadKind::Deployment, "app", "web");
        let rv = block_on(w.scale(&d, "1", 0)).unwrap();
        assert_eq!(rv, "101");
        w.fail_when(|c| matches!(c, Call::Scale { .. }).then_some(WriteError::Conflict));
        assert_eq!(block_on(w.scale(&d, "1", 0)), Err(WriteError::Conflict));
        assert_eq!(w.calls().len(), 2);
        assert!(!w.calls()[0].only_reaper_metadata());
    }
}
