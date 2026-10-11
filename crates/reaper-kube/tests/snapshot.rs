//! Snapshot mapping contract: Kubernetes objects (scrubbed live shapes in
//! `tests/fixtures/`) to the core's plain views. No API server involved.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::DynamicObject;
use reaper_core::Timestamp;
use reaper_core::model::{
    ContainerView, FluxRefs, HelmReleaseView, NamespaceView, NsName, OwnerRef, PodView,
    Termination, WorkloadKind, WorkloadRef, WorkloadView,
};
use reaper_flux::helmrelease::HelmRelease;
use reaper_kube::snapshot::{self, Objects};
use serde::de::DeserializeOwned;
use serde_json::json;

fn fixture<T: DeserializeOwned>(name: &str) -> T {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn from<T: DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn owner(kind: &str, name: &str) -> OwnerRef {
    OwnerRef {
        kind: kind.into(),
        name: name.into(),
        controller: true,
    }
}

fn term(exit_code: i32, reason: &str) -> Termination {
    Termination {
        exit_code,
        reason: reason.into(),
    }
}

fn fixtures() -> Objects {
    Objects {
        namespaces: vec![
            Arc::new(fixture("namespace-shop-prod.json")),
            Arc::new(fixture("namespace-ledger-dev.json")),
        ],
        pods: vec![
            Arc::new(fixture("pod-imagepull.json")),
            Arc::new(fixture("pod-storefront.json")),
            Arc::new(fixture("pod-crashloop.json")),
            Arc::new(fixture("pod-job.json")),
        ],
        replica_sets: vec![Arc::new(fixture("replicaset-storefront.json"))],
        deployments: vec![Arc::new(fixture("deployment-storefront.json"))],
        helm_releases: vec![Arc::new(fixture("helmrelease-storefront.json"))],
        scaled_objects: vec![Arc::new(fixture("scaledobject-ledger-worker.json"))],
        ..Objects::default()
    }
}

#[test]
fn healthy_helm_managed_deployment() {
    let d: Deployment = fixture("deployment-storefront.json");
    assert_eq!(
        snapshot::deployment_view(&d),
        WorkloadView {
            workload: WorkloadRef::new(WorkloadKind::Deployment, "shop-prod", "storefront"),
            resource_version: "981234".into(),
            spec_replicas: Some(2),
            available_replicas: 2,
            has_controller_owner: false,
            has_hpa: false,
            flux: FluxRefs {
                kustomization: None,
                helm_release: Some(NsName::new("shop-prod", "storefront")),
                reconcile_annotation: None,
                partial_labels: false,
            },
            mode_label: None,
            mode_annotation: Some("report".into()),
            reaper_annotations: BTreeMap::from([
                (
                    "reaper.boop.ninja/crashloop-since".into(),
                    "2026-10-10T07:55:00Z".into()
                ),
                ("reaper.boop.ninja/mode".into(), "report".into()),
            ]),
            state_label: Some("observed".into()),
        }
    );
}

#[test]
fn replica_set_owner_is_its_controlling_deployment() {
    let rs: ReplicaSet = fixture("replicaset-storefront.json");
    assert_eq!(
        snapshot::replica_set_entry(&rs),
        (
            NsName::new("shop-prod", "storefront-7d9f8c6b5d"),
            Some(owner("Deployment", "storefront"))
        )
    );
}

#[test]
fn healthy_pod_has_app_and_init_containers() {
    let pod: Pod = fixture("pod-storefront.json");
    assert_eq!(
        snapshot::pod_view(&pod),
        PodView {
            namespace: "shop-prod".into(),
            name: "storefront-7d9f8c6b5d-q8x4n".into(),
            owner: Some(owner("ReplicaSet", "storefront-7d9f8c6b5d")),
            containers: vec![
                ContainerView {
                    name: "storefront".into(),
                    init: false,
                    ready: true,
                    ..ContainerView::default()
                },
                ContainerView {
                    name: "migrate".into(),
                    init: true,
                    ready: true,
                    terminated: Some(term(0, "Completed")),
                    ..ContainerView::default()
                },
            ],
        }
    );
}

#[test]
fn crash_looping_pod_keeps_last_termination() {
    let pod: Pod = fixture("pod-crashloop.json");
    let view = snapshot::pod_view(&pod);
    assert_eq!(
        view.owner,
        Some(owner("ReplicaSet", "ledger-api-5c8d7f9b6"))
    );
    assert_eq!(
        view.containers,
        vec![
            ContainerView {
                name: "ledger-api".into(),
                init: false,
                ready: false,
                restart_count: 214,
                waiting_reason: Some("CrashLoopBackOff".into()),
                terminated: None,
                last_terminated: Some(term(1, "Error")),
            },
            ContainerView {
                name: "log-shipper".into(),
                ready: true,
                ..ContainerView::default()
            },
        ]
    );
}

#[test]
fn job_pod_is_owned_by_its_job() {
    let pod: Pod = fixture("pod-job.json");
    let view = snapshot::pod_view(&pod);
    assert_eq!(view.owner, Some(owner("Job", "nightly-report-29812340")));
    assert_eq!(
        view.containers,
        vec![ContainerView {
            name: "report".into(),
            terminated: Some(term(2, "Error")),
            ..ContainerView::default()
        }]
    );
}

#[test]
fn image_pull_back_off_pod() {
    let pod: Pod = fixture("pod-imagepull.json");
    let view = snapshot::pod_view(&pod);
    assert_eq!(
        view.containers,
        vec![ContainerView {
            name: "worker".into(),
            waiting_reason: Some("ImagePullBackOff".into()),
            ..ContainerView::default()
        }]
    );
}

#[test]
fn pod_owner_ignores_non_controller_references() {
    let pod: Pod = from(json!({
        "metadata": {
            "name": "p", "namespace": "ns",
            "ownerReferences": [
                {"apiVersion": "v1", "kind": "ConfigMap", "name": "cm", "uid": "u1"},
                {"apiVersion": "apps/v1", "kind": "StatefulSet", "name": "db", "uid": "u2", "controller": true}
            ]
        }
    }));
    let view = snapshot::pod_view(&pod);
    assert_eq!(view.owner, Some(owner("StatefulSet", "db")));
    assert_eq!(view.containers, Vec::<ContainerView>::new());

    let orphan: Pod = from(json!({
        "metadata": {
            "name": "p", "namespace": "ns",
            "ownerReferences": [{"apiVersion": "v1", "kind": "ConfigMap", "name": "cm", "uid": "u1"}]
        }
    }));
    assert_eq!(snapshot::pod_view(&orphan).owner, None);
}

#[test]
fn namespace_mode_label_and_annotation() {
    let ns: Namespace = fixture("namespace-ledger-dev.json");
    assert_eq!(
        snapshot::namespace_view(&ns),
        NamespaceView {
            name: "ledger-dev".into(),
            mode_label: Some("report".into()),
            mode_annotation: Some("off".into()),
        }
    );
    let plain: Namespace = fixture("namespace-shop-prod.json");
    assert_eq!(
        snapshot::namespace_view(&plain),
        NamespaceView {
            name: "shop-prod".into(),
            ..NamespaceView::default()
        }
    );
}

#[test]
fn helm_release_view() {
    let hr: HelmRelease = fixture("helmrelease-storefront.json");
    assert_eq!(
        snapshot::helm_release_view(&hr),
        HelmReleaseView {
            helm_release: NsName::new("shop-prod", "storefront"),
            resource_version: "980011".into(),
            suspended: true,
            kustomization: Some(NsName::new("flux-system", "apps")),
            reconcile_annotation: Some("enabled".into()),
            reaper_annotations: BTreeMap::from([(
                "reaper.boop.ninja/held-for".into(),
                "[\"Deployment/shop-prod/storefront\"]".into()
            )]),
        }
    );
}

#[test]
fn helm_release_without_suspend_is_not_suspended() {
    let hr: HelmRelease = from(json!({
        "apiVersion": "helm.toolkit.fluxcd.io/v2",
        "kind": "HelmRelease",
        "metadata": {"name": "web", "namespace": "apps", "resourceVersion": "9"},
        "spec": {"interval": "5m", "chart": {"spec": {"chart": "web"}}}
    }));
    let view = snapshot::helm_release_view(&hr);
    assert!(!view.suspended);
    assert_eq!(view.kustomization, None);
}

#[test]
fn flux_label_pairs() {
    let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let annotations = labels(&[("kustomize.toolkit.fluxcd.io/reconcile", "disabled")]);

    let both = labels(&[
        ("kustomize.toolkit.fluxcd.io/name", "apps"),
        ("kustomize.toolkit.fluxcd.io/namespace", "flux-system"),
    ]);
    assert_eq!(
        snapshot::flux_refs(Some(&both), Some(&annotations)),
        FluxRefs {
            kustomization: Some(NsName::new("flux-system", "apps")),
            helm_release: None,
            reconcile_annotation: Some("disabled".into()),
            partial_labels: false,
        }
    );

    let partial = labels(&[
        ("kustomize.toolkit.fluxcd.io/name", "apps"),
        ("kustomize.toolkit.fluxcd.io/namespace", "flux-system"),
        ("helm.toolkit.fluxcd.io/name", "web"),
    ]);
    let refs = snapshot::flux_refs(Some(&partial), None);
    assert_eq!(refs.kustomization, Some(NsName::new("flux-system", "apps")));
    assert_eq!(refs.helm_release, None);
    assert!(refs.partial_labels);

    assert_eq!(snapshot::flux_refs(None, None), FluxRefs::default());
}

#[test]
fn deployment_with_controller_owner_and_default_replicas() {
    let d: Deployment = from(json!({
        "metadata": {
            "name": "web", "namespace": "apps", "resourceVersion": "7",
            "ownerReferences": [
                {"apiVersion": "example.test/v1", "kind": "WebApp", "name": "web", "uid": "u", "controller": true}
            ]
        },
        "spec": {"selector": {}, "template": {}}
    }));
    let view = snapshot::deployment_view(&d);
    assert!(view.has_controller_owner);
    assert_eq!(view.spec_replicas, Some(1));
    assert_eq!(view.available_replicas, 0);

    let referenced_only: Deployment = from(json!({
        "metadata": {
            "name": "web", "namespace": "apps",
            "ownerReferences": [{"apiVersion": "v1", "kind": "ConfigMap", "name": "c", "uid": "u"}]
        }
    }));
    assert!(!snapshot::deployment_view(&referenced_only).has_controller_owner);
}

#[test]
fn stateful_set_uses_ready_replicas() {
    let s: StatefulSet = from(json!({
        "metadata": {
            "name": "db", "namespace": "data", "resourceVersion": "3",
            "labels": {"reaper.boop.ninja/mode": "off"}
        },
        "spec": {"replicas": 3, "serviceName": "db", "selector": {}, "template": {}},
        "status": {"replicas": 3, "readyReplicas": 1, "availableReplicas": 3}
    }));
    let view = snapshot::stateful_set_view(&s);
    assert_eq!(
        view.workload,
        WorkloadRef::new(WorkloadKind::StatefulSet, "data", "db")
    );
    assert_eq!(view.spec_replicas, Some(3));
    assert_eq!(view.available_replicas, 1);
    assert_eq!(view.mode_label.as_deref(), Some("off"));
}

#[test]
fn daemon_set_has_no_spec_replicas() {
    let ds: DaemonSet = from(json!({
        "metadata": {"name": "agent", "namespace": "infra", "resourceVersion": "5"},
        "spec": {"selector": {}, "template": {}},
        "status": {
            "currentNumberScheduled": 4, "desiredNumberScheduled": 4,
            "numberMisscheduled": 0, "numberReady": 3, "numberAvailable": 2
        }
    }));
    let view = snapshot::daemon_set_view(&ds);
    assert_eq!(
        view.workload,
        WorkloadRef::new(WorkloadKind::DaemonSet, "infra", "agent")
    );
    assert_eq!(view.spec_replicas, None);
    assert_eq!(view.available_replicas, 2);
}

#[test]
fn keda_scale_target_defaults_to_deployment() {
    let so: DynamicObject = fixture("scaledobject-ledger-worker.json");
    assert_eq!(
        snapshot::keda_target(&so),
        Some(WorkloadRef::new(
            WorkloadKind::Deployment,
            "ledger-dev",
            "ledger-worker"
        ))
    );
    let sts: DynamicObject = from(json!({
        "apiVersion": "keda.sh/v1alpha1", "kind": "ScaledObject",
        "metadata": {"name": "db", "namespace": "data"},
        "spec": {"scaleTargetRef": {"apiVersion": "apps/v1", "kind": "StatefulSet", "name": "db"}}
    }));
    assert_eq!(
        snapshot::keda_target(&sts),
        Some(WorkloadRef::new(WorkloadKind::StatefulSet, "data", "db"))
    );
    let custom: DynamicObject = from(json!({
        "apiVersion": "keda.sh/v1alpha1", "kind": "ScaledObject",
        "metadata": {"name": "x", "namespace": "data"},
        "spec": {"scaleTargetRef": {"apiVersion": "example.test/v1", "kind": "Rollout", "name": "x"}}
    }));
    assert_eq!(snapshot::keda_target(&custom), None);
}

#[test]
fn hpa_marks_only_its_scale_target() {
    let deployment = |name: &str| -> Arc<Deployment> {
        Arc::new(from(json!({
            "metadata": {"name": name, "namespace": "apps", "resourceVersion": "1"}
        })))
    };
    let hpa: HorizontalPodAutoscaler = from(json!({
        "metadata": {"name": "web", "namespace": "apps"},
        "spec": {
            "maxReplicas": 5,
            "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "web"}
        }
    }));
    let other_ns: HorizontalPodAutoscaler = from(json!({
        "metadata": {"name": "api", "namespace": "elsewhere"},
        "spec": {
            "maxReplicas": 5,
            "scaleTargetRef": {"apiVersion": "apps/v1", "kind": "Deployment", "name": "api"}
        }
    }));
    let objects = Objects {
        deployments: vec![deployment("web"), deployment("api")],
        hpas: vec![Arc::new(hpa), Arc::new(other_ns)],
        ..Objects::default()
    };
    let snap = snapshot::build_snapshot(&objects, Timestamp(0), "reaper");
    let with_hpa: Vec<(String, bool)> = snap
        .workloads
        .iter()
        .map(|w| (w.workload.name.clone(), w.has_hpa))
        .collect();
    assert_eq!(
        with_hpa,
        vec![("api".to_owned(), false), ("web".to_owned(), true)]
    );
}

#[test]
fn snapshot_from_fixtures() {
    let now = Timestamp(1_791_619_200);
    let snap = snapshot::build_snapshot(&fixtures(), now, "k8s-reaper");
    assert_eq!(snap.now, now);
    assert_eq!(snap.reaper_namespace, "k8s-reaper");
    assert_eq!(
        snap.namespaces.keys().cloned().collect::<Vec<_>>(),
        vec!["ledger-dev".to_owned(), "shop-prod".to_owned()]
    );
    assert_eq!(
        snap.workloads
            .iter()
            .map(|w| w.workload.to_string())
            .collect::<Vec<_>>(),
        vec!["Deployment/shop-prod/storefront".to_owned()]
    );
    assert_eq!(
        snap.replica_sets,
        BTreeMap::from([(
            NsName::new("shop-prod", "storefront-7d9f8c6b5d"),
            Some(owner("Deployment", "storefront"))
        )])
    );
    assert_eq!(
        snap.pods
            .iter()
            .map(|p| format!("{}/{}", p.namespace, p.name))
            .collect::<Vec<_>>(),
        vec![
            "ledger-dev/ledger-api-5c8d7f9b6-x2kqp",
            "ledger-dev/ledger-worker-6f7b8c9d4-m3pzt",
            "shop-prod/nightly-report-29812340-7wq2z",
            "shop-prod/storefront-7d9f8c6b5d-q8x4n",
        ]
    );
    assert_eq!(
        snap.helm_releases.keys().cloned().collect::<Vec<_>>(),
        vec![NsName::new("shop-prod", "storefront")]
    );
    assert_eq!(
        snap.keda_targets,
        BTreeSet::from([WorkloadRef::new(
            WorkloadKind::Deployment,
            "ledger-dev",
            "ledger-worker"
        )])
    );
}

#[test]
fn snapshot_without_keda_or_flux_is_empty_there() {
    let objects = Objects {
        scaled_objects: Vec::new(),
        helm_releases: Vec::new(),
        ..fixtures()
    };
    let snap = snapshot::build_snapshot(&objects, Timestamp(0), "k8s-reaper");
    assert_eq!(snap.keda_targets, BTreeSet::new());
    assert_eq!(snap.helm_releases, BTreeMap::new());
}
