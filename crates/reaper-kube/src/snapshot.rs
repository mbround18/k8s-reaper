//! Watch-backed caches and the mapping from Kubernetes objects to the core's
//! [`Snapshot`].
//!
//! The mapping functions are pure (objects in, views out) and are what the
//! contract tests exercise. [`Caches`] only owns the reflectors and hands
//! their contents to [`build_snapshot`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{ContainerState, ContainerStatus, Namespace, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::api::{Api, DynamicObject};
use kube::core::{ApiResource, GroupVersionKind};
use kube::runtime::reflector::{self, Store, store::Writer};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Client, Resource, ResourceExt};
use reaper_core::Timestamp;
use reaper_core::decide::wants_immediate_cycle;
use reaper_core::model::{
    ContainerView, CronJobView, FluxRefs, HelmReleaseView, JobOutcome, JobView, NamespaceView,
    NsName, ObjectKind, ObjectRef, OwnerRef, PodView, Snapshot, Termination, WorkloadKind,
    WorkloadRef, WorkloadView,
};
use reaper_core::record::{FLUX_RECONCILE, MODE_KEY, PREFIX, STATE_LABEL};
use reaper_flux::helmrelease::{self, HelmRelease};
use serde::de::DeserializeOwned;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const KUSTOMIZE_NAME: &str = "kustomize.toolkit.fluxcd.io/name";
const KUSTOMIZE_NAMESPACE: &str = "kustomize.toolkit.fluxcd.io/namespace";
const HELM_NAME: &str = "helm.toolkit.fluxcd.io/name";
const HELM_NAMESPACE: &str = "helm.toolkit.fluxcd.io/namespace";

type Labels = BTreeMap<String, String>;

/// The cached objects one snapshot is built from.
#[derive(Debug, Clone, Default)]
pub struct Objects {
    pub namespaces: Vec<Arc<Namespace>>,
    pub pods: Vec<Arc<Pod>>,
    pub replica_sets: Vec<Arc<ReplicaSet>>,
    pub deployments: Vec<Arc<Deployment>>,
    pub stateful_sets: Vec<Arc<StatefulSet>>,
    pub daemon_sets: Vec<Arc<DaemonSet>>,
    pub cron_jobs: Vec<Arc<CronJob>>,
    pub jobs: Vec<Arc<Job>>,
    pub hpas: Vec<Arc<HorizontalPodAutoscaler>>,
    /// KEDA `ScaledObject`s; empty when the CRD is absent.
    pub scaled_objects: Vec<Arc<DynamicObject>>,
    /// Empty when the Flux HelmRelease CRD is absent.
    pub helm_releases: Vec<Arc<HelmRelease>>,
}

/// Builds the core's snapshot. Workloads and pods are sorted for determinism.
#[must_use]
pub fn build_snapshot(objects: &Objects, now: Timestamp, reaper_namespace: &str) -> Snapshot {
    let hpa_targets: BTreeSet<WorkloadRef> =
        objects.hpas.iter().filter_map(|h| hpa_target(h)).collect();
    let mut jobs_by_cron_job: BTreeMap<NsName, Vec<&Job>> = BTreeMap::new();
    for job in &objects.jobs {
        if let Some(owner) = controller_owner(&job.metadata).filter(|o| o.kind == "CronJob") {
            let ns = job.metadata.namespace.clone().unwrap_or_default();
            jobs_by_cron_job
                .entry(NsName::new(ns, owner.name))
                .or_default()
                .push(job);
        }
    }

    let mut workloads: Vec<WorkloadView> = objects
        .deployments
        .iter()
        .map(|d| deployment_view(d))
        .chain(objects.stateful_sets.iter().map(|s| stateful_set_view(s)))
        .chain(objects.daemon_sets.iter().map(|d| daemon_set_view(d)))
        .chain(objects.cron_jobs.iter().map(|c| {
            let jobs = jobs_by_cron_job.get(&ns_name(&c.metadata));
            cron_job_view(c, jobs.map(Vec::as_slice).unwrap_or_default())
        }))
        .map(|mut w| {
            w.has_hpa = hpa_targets.contains(&w.workload);
            w
        })
        .collect();
    workloads.sort_by(|a, b| a.workload.cmp(&b.workload));

    let mut pods: Vec<PodView> = objects.pods.iter().map(|p| pod_view(p)).collect();
    pods.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    Snapshot {
        now,
        reaper_namespace: reaper_namespace.to_owned(),
        namespaces: objects
            .namespaces
            .iter()
            .map(|n| {
                let v = namespace_view(n);
                (v.name.clone(), v)
            })
            .collect(),
        workloads,
        replica_sets: objects
            .replica_sets
            .iter()
            .map(|rs| replica_set_entry(rs))
            .collect(),
        pods,
        helm_releases: objects
            .helm_releases
            .iter()
            .map(|hr| {
                let v = helm_release_view(hr);
                (v.helm_release.clone(), v)
            })
            .collect(),
        keda_targets: objects
            .scaled_objects
            .iter()
            .filter_map(|so| keda_target(so))
            .collect(),
    }
}

#[must_use]
pub fn namespace_view(ns: &Namespace) -> NamespaceView {
    NamespaceView {
        name: ns.metadata.name.clone().unwrap_or_default(),
        mode_label: get(ns.metadata.labels.as_ref(), MODE_KEY),
        mode_annotation: get(ns.metadata.annotations.as_ref(), MODE_KEY),
    }
}

/// `has_hpa` is left false; [`build_snapshot`] fills it.
#[must_use]
pub fn deployment_view(d: &Deployment) -> WorkloadView {
    let spec = d.spec.as_ref().and_then(|s| s.replicas);
    let available = d.status.as_ref().and_then(|s| s.available_replicas);
    workload_view(
        WorkloadKind::Deployment,
        &d.metadata,
        Some(spec.map_or(1, count)),
        available.map_or(0, count),
    )
}

/// `has_hpa` is left false; [`build_snapshot`] fills it.
#[must_use]
pub fn stateful_set_view(s: &StatefulSet) -> WorkloadView {
    let spec = s.spec.as_ref().and_then(|s| s.replicas);
    let ready = s.status.as_ref().and_then(|s| s.ready_replicas);
    workload_view(
        WorkloadKind::StatefulSet,
        &s.metadata,
        Some(spec.map_or(1, count)),
        ready.map_or(0, count),
    )
}

#[must_use]
pub fn daemon_set_view(d: &DaemonSet) -> WorkloadView {
    let available = d.status.as_ref().and_then(|s| s.number_available);
    workload_view(
        WorkloadKind::DaemonSet,
        &d.metadata,
        None,
        available.map_or(0, count),
    )
}

/// `jobs` are the Jobs this CronJob controls; [`build_snapshot`] groups them.
#[must_use]
pub fn cron_job_view(c: &CronJob, jobs: &[&Job]) -> WorkloadView {
    let mut jobs: Vec<JobView> = jobs.iter().map(|j| job_view(j)).collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut w = workload_view(WorkloadKind::CronJob, &c.metadata, None, 0);
    w.cron_job = Some(CronJobView {
        suspended: c.spec.suspend.unwrap_or(false),
        last_successful_time: c
            .status
            .as_ref()
            .and_then(|s| s.last_successful_time.as_ref())
            .map(timestamp),
        jobs,
    });
    w
}

/// A Job is finished once its `Complete` or `Failed` condition is true.
#[must_use]
pub fn job_view(job: &Job) -> JobView {
    let status = job.status.as_ref();
    let conditions = status
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default();
    let true_condition = |type_: &str| {
        conditions
            .iter()
            .find(|c| c.type_ == type_ && c.status == "True")
    };
    let outcome = if let Some(c) = true_condition("Failed") {
        JobOutcome::Failed {
            at: c
                .last_transition_time
                .as_ref()
                .map_or(Timestamp(0), timestamp),
            reason: c.reason.clone(),
        }
    } else if let Some(c) = true_condition("Complete") {
        let at = status
            .and_then(|s| s.completion_time.as_ref())
            .or(c.last_transition_time.as_ref());
        JobOutcome::Succeeded {
            at: at.map_or(Timestamp(0), timestamp),
        }
    } else {
        JobOutcome::Running
    };
    JobView {
        name: job.metadata.name.clone().unwrap_or_default(),
        outcome,
    }
}

fn timestamp(t: &Time) -> Timestamp {
    Timestamp(t.0.as_second())
}

fn workload_view(
    kind: WorkloadKind,
    meta: &ObjectMeta,
    spec_replicas: Option<u32>,
    available_replicas: u32,
) -> WorkloadView {
    let labels = meta.labels.as_ref();
    let annotations = meta.annotations.as_ref();
    WorkloadView {
        workload: WorkloadRef::new(
            kind,
            meta.namespace.clone().unwrap_or_default(),
            meta.name.clone().unwrap_or_default(),
        ),
        resource_version: meta.resource_version.clone().unwrap_or_default(),
        spec_replicas,
        available_replicas,
        has_controller_owner: controller_owner(meta).is_some(),
        has_hpa: false,
        flux: flux_refs(labels, annotations),
        mode_label: get(labels, MODE_KEY),
        mode_annotation: get(annotations, MODE_KEY),
        reaper_annotations: reaper_annotations(annotations),
        state_label: get(labels, STATE_LABEL),
        cron_job: None,
    }
}

/// A ReplicaSet and its controlling owner.
#[must_use]
pub fn replica_set_entry(rs: &ReplicaSet) -> (NsName, Option<OwnerRef>) {
    (ns_name(&rs.metadata), controller_owner(&rs.metadata))
}

/// App containers first, then init containers.
#[must_use]
pub fn pod_view(pod: &Pod) -> PodView {
    let status = pod.status.as_ref();
    let app = status.and_then(|s| s.container_statuses.as_deref());
    let init = status.and_then(|s| s.init_container_statuses.as_deref());
    let containers = app
        .unwrap_or_default()
        .iter()
        .map(|c| container_view(c, false))
        .chain(
            init.unwrap_or_default()
                .iter()
                .map(|c| container_view(c, true)),
        )
        .collect();
    PodView {
        namespace: pod.metadata.namespace.clone().unwrap_or_default(),
        name: pod.metadata.name.clone().unwrap_or_default(),
        owner: controller_owner(&pod.metadata),
        containers,
    }
}

fn container_view(c: &ContainerStatus, init: bool) -> ContainerView {
    ContainerView {
        name: c.name.clone(),
        init,
        ready: c.ready,
        restart_count: count(c.restart_count),
        waiting_reason: c
            .state
            .as_ref()
            .and_then(|s| s.waiting.as_ref())
            .and_then(|w| w.reason.clone()),
        terminated: termination(c.state.as_ref()),
        last_terminated: termination(c.last_state.as_ref()),
    }
}

fn termination(state: Option<&ContainerState>) -> Option<Termination> {
    state
        .and_then(|s| s.terminated.as_ref())
        .map(|t| Termination {
            exit_code: t.exit_code,
            reason: t.reason.clone().unwrap_or_default(),
        })
}

#[must_use]
pub fn helm_release_view(hr: &HelmRelease) -> HelmReleaseView {
    let annotations = hr.metadata.annotations.as_ref();
    HelmReleaseView {
        helm_release: ns_name(&hr.metadata),
        resource_version: hr.metadata.resource_version.clone().unwrap_or_default(),
        suspended: hr.spec.suspend,
        kustomization: label_pair(
            hr.metadata.labels.as_ref(),
            KUSTOMIZE_NAME,
            KUSTOMIZE_NAMESPACE,
        )
        .0,
        reconcile_annotation: get(annotations, FLUX_RECONCILE),
        reaper_annotations: reaper_annotations(annotations),
    }
}

/// Flux ownership from an object's labels and its current reconcile annotation.
#[must_use]
pub fn flux_refs(labels: Option<&Labels>, annotations: Option<&Labels>) -> FluxRefs {
    let (kustomization, k_partial) = label_pair(labels, KUSTOMIZE_NAME, KUSTOMIZE_NAMESPACE);
    let (helm_release, h_partial) = label_pair(labels, HELM_NAME, HELM_NAMESPACE);
    FluxRefs {
        kustomization,
        helm_release,
        reconcile_annotation: get(annotations, FLUX_RECONCILE),
        partial_labels: k_partial || h_partial,
    }
}

/// The pair as an `NsName` when both labels are present; `true` when only one is.
fn label_pair(labels: Option<&Labels>, name: &str, namespace: &str) -> (Option<NsName>, bool) {
    match (get(labels, name), get(labels, namespace)) {
        (Some(n), Some(ns)) => (Some(NsName::new(ns, n)), false),
        (None, None) => (None, false),
        _ => (None, true),
    }
}

/// The workload a KEDA `ScaledObject` scales (`kind` defaults to Deployment).
#[must_use]
pub fn keda_target(so: &DynamicObject) -> Option<WorkloadRef> {
    let target = so.data.get("spec")?.get("scaleTargetRef")?;
    let name = target.get("name")?.as_str()?;
    let kind = target
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("Deployment");
    scalable(kind, so.metadata.namespace.as_deref()?, name)
}

fn hpa_target(hpa: &HorizontalPodAutoscaler) -> Option<WorkloadRef> {
    let target = &hpa.spec.scale_target_ref;
    scalable(
        &target.kind,
        hpa.metadata.namespace.as_deref()?,
        &target.name,
    )
}

fn scalable(kind: &str, namespace: &str, name: &str) -> Option<WorkloadRef> {
    match kind.parse().ok()? {
        k @ (WorkloadKind::Deployment | WorkloadKind::StatefulSet) => {
            Some(WorkloadRef::new(k, namespace, name))
        }
        _ => None,
    }
}

fn controller_owner(meta: &ObjectMeta) -> Option<OwnerRef> {
    meta.owner_references
        .as_deref()?
        .iter()
        .find(|o| o.controller == Some(true))
        .map(|o| OwnerRef {
            kind: o.kind.clone(),
            name: o.name.clone(),
            controller: true,
        })
}

fn ns_name(meta: &ObjectMeta) -> NsName {
    NsName::new(
        meta.namespace.clone().unwrap_or_default(),
        meta.name.clone().unwrap_or_default(),
    )
}

fn get(map: Option<&Labels>, key: &str) -> Option<String> {
    map?.get(key).cloned()
}

fn reaper_annotations(annotations: Option<&Labels>) -> Labels {
    annotations
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.starts_with(PREFIX))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn count(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// Reflector-backed caches for every kind the snapshot reads. Watch tasks are
/// aborted when this is dropped.
pub struct Caches {
    reaper_namespace: String,
    namespaces: Store<Namespace>,
    pods: Store<Pod>,
    replica_sets: Store<ReplicaSet>,
    deployments: Store<Deployment>,
    stateful_sets: Store<StatefulSet>,
    daemon_sets: Store<DaemonSet>,
    cron_jobs: Store<CronJob>,
    jobs: Store<Job>,
    hpas: Store<HorizontalPodAutoscaler>,
    scaled_objects: Option<Store<DynamicObject>>,
    helm_releases: Option<Store<HelmRelease>>,
    trigger: Arc<Notify>,
    tasks: Vec<JoinHandle<()>>,
}

impl Caches {
    /// Discovers the optional CRDs (KEDA `ScaledObject`, Flux HelmRelease) and
    /// starts one watch per kind. Must run inside a Tokio runtime.
    pub async fn start(client: Client, reaper_namespace: impl Into<String>) -> kube::Result<Self> {
        let keda = discover(
            &client,
            &GroupVersionKind::gvk("keda.sh", "v1alpha1", "ScaledObject"),
        )
        .await?;
        let flux = discover(&client, &helmrelease::gvk()).await?;
        tracing::info!(
            keda = keda.is_some(),
            flux_helm = flux.is_some(),
            "optional CRDs discovered"
        );

        let mut tasks = Vec::new();
        let scaled_objects = keda.map(|ar| {
            let writer = Writer::new(ar.clone());
            let store = writer.as_reader();
            tasks.push(watch(Api::all_with(client.clone(), &ar), writer));
            store
        });
        let helm_releases = flux.map(|_| typed(&client, &mut tasks));
        let trigger = Arc::new(Notify::new());

        Ok(Caches {
            reaper_namespace: reaper_namespace.into(),
            namespaces: typed(&client, &mut tasks),
            pods: typed(&client, &mut tasks),
            replica_sets: typed(&client, &mut tasks),
            deployments: triggering(&client, &mut tasks, &trigger, deployment_view),
            stateful_sets: triggering(&client, &mut tasks, &trigger, stateful_set_view),
            daemon_sets: typed(&client, &mut tasks),
            cron_jobs: triggering(&client, &mut tasks, &trigger, |c| cron_job_view(c, &[])),
            jobs: typed(&client, &mut tasks),
            hpas: typed(&client, &mut tasks),
            scaled_objects,
            helm_releases,
            trigger,
            tasks,
        })
    }

    #[must_use]
    pub fn keda_enabled(&self) -> bool {
        self.scaled_objects.is_some()
    }

    #[must_use]
    pub fn flux_enabled(&self) -> bool {
        self.helm_releases.is_some()
    }

    /// Resolves when a watched change wants a cycle before the next tick
    /// ([`wants_immediate_cycle`]). Changes while no one waits are kept as
    /// one pending wake-up.
    pub async fn triggered(&self) {
        self.trigger.notified().await;
    }

    /// Resolves once every cache has completed its initial list.
    pub async fn wait_until_synced(&self) -> Result<(), reflector::store::WriterDropped> {
        self.namespaces.wait_until_ready().await?;
        self.pods.wait_until_ready().await?;
        self.replica_sets.wait_until_ready().await?;
        self.deployments.wait_until_ready().await?;
        self.stateful_sets.wait_until_ready().await?;
        self.daemon_sets.wait_until_ready().await?;
        self.cron_jobs.wait_until_ready().await?;
        self.jobs.wait_until_ready().await?;
        self.hpas.wait_until_ready().await?;
        if let Some(s) = &self.scaled_objects {
            s.wait_until_ready().await?;
        }
        if let Some(s) = &self.helm_releases {
            s.wait_until_ready().await?;
        }
        Ok(())
    }

    /// The current cache contents.
    #[must_use]
    pub fn objects(&self) -> Objects {
        Objects {
            namespaces: self.namespaces.state(),
            pods: self.pods.state(),
            replica_sets: self.replica_sets.state(),
            deployments: self.deployments.state(),
            stateful_sets: self.stateful_sets.state(),
            daemon_sets: self.daemon_sets.state(),
            cron_jobs: self.cron_jobs.state(),
            jobs: self.jobs.state(),
            hpas: self.hpas.state(),
            scaled_objects: self
                .scaled_objects
                .as_ref()
                .map(Store::state)
                .unwrap_or_default(),
            helm_releases: self
                .helm_releases
                .as_ref()
                .map(Store::state)
                .unwrap_or_default(),
        }
    }

    /// A uid lookup over these caches, for Events' `regarding.uid`.
    #[must_use]
    pub fn uids(&self) -> Uids {
        Uids {
            namespaces: self.namespaces.clone(),
            pods: self.pods.clone(),
            deployments: self.deployments.clone(),
            stateful_sets: self.stateful_sets.clone(),
            daemon_sets: self.daemon_sets.clone(),
            cron_jobs: self.cron_jobs.clone(),
            helm_releases: self.helm_releases.clone(),
        }
    }

    #[must_use]
    pub fn snapshot(&self, now: Timestamp) -> Snapshot {
        build_snapshot(&self.objects(), now, &self.reaper_namespace)
    }
}

/// Finds the uid of a cached object. The `ReaperConfig` is not cached and has
/// none.
#[derive(Clone)]
pub struct Uids {
    namespaces: Store<Namespace>,
    pods: Store<Pod>,
    deployments: Store<Deployment>,
    stateful_sets: Store<StatefulSet>,
    daemon_sets: Store<DaemonSet>,
    cron_jobs: Store<CronJob>,
    helm_releases: Option<Store<HelmRelease>>,
}

impl Uids {
    #[must_use]
    pub fn get(&self, target: &ObjectRef) -> Option<String> {
        match target.kind {
            ObjectKind::Namespace => uid_of(&self.namespaces, target),
            ObjectKind::Pod => uid_of(&self.pods, target),
            ObjectKind::Deployment => uid_of(&self.deployments, target),
            ObjectKind::StatefulSet => uid_of(&self.stateful_sets, target),
            ObjectKind::DaemonSet => uid_of(&self.daemon_sets, target),
            ObjectKind::HelmRelease => self.helm_releases.as_ref().and_then(|s| uid_of(s, target)),
            ObjectKind::CronJob => uid_of(&self.cron_jobs, target),
            ObjectKind::ReaperConfig => None,
        }
    }
}

fn uid_of<K>(store: &Store<K>, target: &ObjectRef) -> Option<String>
where
    K: Resource<DynamicType = ()> + Clone,
{
    let mut key = reflector::ObjectRef::<K>::new(&target.name);
    if let Some(ns) = &target.namespace {
        key = key.within(ns);
    }
    store.get(&key).and_then(|obj| obj.meta().uid.clone())
}

impl Drop for Caches {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// `None` when the kind is not served by the cluster.
async fn discover(client: &Client, gvk: &GroupVersionKind) -> kube::Result<Option<ApiResource>> {
    match kube::discovery::pinned_kind(client, gvk).await {
        Ok((ar, _)) => Ok(Some(ar)),
        Err(kube::Error::Discovery(_)) => Ok(None),
        Err(kube::Error::Api(s)) if s.code == 404 => Ok(None),
        Err(e) => Err(e),
    }
}

fn typed<K>(client: &Client, tasks: &mut Vec<JoinHandle<()>>) -> Store<K>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
{
    let (store, writer) = reflector::store();
    tasks.push(watch(Api::all(client.clone()), writer));
    store
}

/// Like [`typed`], and wakes `trigger` when an update (not the initial list)
/// leaves a workload that wants a cycle now.
fn triggering<K>(
    client: &Client,
    tasks: &mut Vec<JoinHandle<()>>,
    trigger: &Arc<Notify>,
    view: fn(&K) -> WorkloadView,
) -> Store<K>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
{
    let (store, writer) = reflector::store();
    let trigger = Arc::clone(trigger);
    let on_apply = move |obj: &K| {
        if wants_immediate_cycle(&view(obj)) {
            trigger.notify_one();
        }
    };
    tasks.push(watch_with(Api::all(client.clone()), writer, on_apply));
    store
}

fn watch<K>(api: Api<K>, writer: Writer<K>) -> JoinHandle<()>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
    K::DynamicType: Eq + Hash + Clone + Send + Sync,
{
    watch_with(api, writer, |_: &K| {})
}

fn watch_with<K>(
    api: Api<K>,
    writer: Writer<K>,
    on_apply: impl Fn(&K) + Send + 'static,
) -> JoinHandle<()>
where
    K: Resource + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
    K::DynamicType: Eq + Hash + Clone + Send + Sync,
{
    let stream = watcher(api, watcher::Config::default().any_semantic())
        .default_backoff()
        .modify(|o| o.managed_fields_mut().clear());
    tokio::spawn(reflector::reflector(writer, stream).for_each(move |event| {
        match event {
            Ok(watcher::Event::Apply(obj)) => on_apply(&obj),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "watch error; retrying with backoff"),
        }
        futures::future::ready(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store<K>(objects: Vec<K>) -> Store<K>
    where
        K: Resource<DynamicType = ()> + Clone,
    {
        let (store, mut writer) = reflector::store();
        for obj in objects {
            writer.apply_watcher_event(&watcher::Event::Apply(obj));
        }
        store
    }

    fn meta(namespace: Option<&str>, name: &str, uid: &str) -> ObjectMeta {
        ObjectMeta {
            namespace: namespace.map(str::to_owned),
            name: Some(name.to_owned()),
            uid: Some(uid.to_owned()),
            ..ObjectMeta::default()
        }
    }

    #[test]
    fn uids_come_from_the_caches() {
        let uids = Uids {
            namespaces: store(vec![Namespace {
                metadata: meta(None, "shop", "ns-1"),
                ..Namespace::default()
            }]),
            pods: store(vec![]),
            deployments: store(vec![Deployment {
                metadata: meta(Some("shop"), "web", "d-1"),
                ..Deployment::default()
            }]),
            stateful_sets: store(vec![]),
            daemon_sets: store(vec![]),
            cron_jobs: store(vec![]),
            helm_releases: None,
        };
        let web = WorkloadRef::new(WorkloadKind::Deployment, "shop", "web").object_ref();
        assert_eq!(uids.get(&web).as_deref(), Some("d-1"));
        let other = WorkloadRef::new(WorkloadKind::Deployment, "other", "web").object_ref();
        assert_eq!(uids.get(&other), None);
        let ns = ObjectRef {
            kind: ObjectKind::Namespace,
            namespace: None,
            name: "shop".into(),
        };
        assert_eq!(uids.get(&ns).as_deref(), Some("ns-1"));
        assert_eq!(
            uids.get(&ObjectRef::helm_release(&NsName::new("shop", "web"))),
            None
        );
    }
}
