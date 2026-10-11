//! US3 on the dev cluster (quickstart §4, SC-003). Opt-in:
//!
//! ```sh
//! make dev-kind image dev-load dev-run KUBE_CONTEXT=kind-...  # or: make dev-cluster && make image dev-load dev-run
//! make test-integration STORY=us3_restore KUBE_CONTEXT=kind-...
//! ```
//!
//! Switches the dev ReaperConfig to full mode, scoped to the plain, kustomize,
//! helm and partial fixtures, lets the reaper reap the three crash-looping
//! fixtures, restores them, and puts the ReaperConfig back.

mod common;
mod full;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use full::{FIXTURES, FullMode, NAME, annotation, is_reaped, replicas, until};
use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{DynamicObject, Patch, PatchParams};
use kube::{Api, Client, Resource};
use reaper_core::record::{HELD_FOR, ORIGINAL_REPLICAS, is_owned_key};
use serde_json::{Value, json};

/// The cycle interval while the story runs. Long enough that a restore
/// finishing well inside it was started by the watch (T054), not the tick.
const CYCLE: &str = "2m";
const RESTORE_WITHIN: Duration = Duration::from_secs(20);

/// What SC-003 compares: everything a person or Flux set, without the keys
/// the reaper owns (it writes `observed` again on the next cycle, because the
/// fixtures still crash).
#[derive(Debug, PartialEq)]
struct Shape {
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    spec: Value,
}

fn shape<K: Resource>(obj: &K, mut spec: Value) -> Shape {
    let strip = |m: &Option<BTreeMap<String, String>>| {
        m.clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| !is_owned_key(k))
            .collect()
    };
    // Flux reads an absent `suspend` as false; the release writes false.
    if spec["suspend"] == false {
        spec.as_object_mut().unwrap().remove("suspend");
    }
    Shape {
        labels: strip(&obj.meta().labels),
        annotations: strip(&obj.meta().annotations),
        spec,
    }
}

fn deployment_shape(d: &Deployment) -> Shape {
    shape(d, serde_json::to_value(d.spec.as_ref().unwrap()).unwrap())
}

fn helm_release_shape(hr: &DynamicObject) -> Shape {
    shape(hr, hr.data["spec"].clone())
}

async fn snapshot(client: &Client) -> (BTreeMap<&'static str, Shape>, Shape) {
    let mut deployments = BTreeMap::new();
    for ns in FIXTURES {
        let d: Deployment = Api::namespaced(client.clone(), ns).get(NAME).await.unwrap();
        deployments.insert(ns, deployment_shape(&d));
    }
    let hr = full::helm_releases(client).get(NAME).await.unwrap();
    (deployments, helm_release_shape(&hr))
}

fn is_restored(d: &Deployment) -> bool {
    annotation(d, ORIGINAL_REPLICAS).is_none() && replicas(d) == Some(2)
}

#[tokio::test]
#[ignore = "needs the dev cluster with Flux and the reaper running (make dev-kind dev-run)"]
async fn restore_puts_back_exactly_what_the_reap_changed() {
    let client = common::dev_client().await;
    full::restore_leftovers(&client).await;
    let (before, hr_before) = snapshot(&client).await;

    let full_mode = FullMode::enter(&client, CYCLE).await;
    for ns in FIXTURES {
        until(&client, ns, full::DEADLINE, "reaped", is_reaped).await;
    }
    let hr = full::helm_releases(&client).get(NAME).await.unwrap();
    assert!(full::suspended(&hr) && annotation(&hr, HELD_FOR).is_some());

    // Scenario 1: the restore annotation, on kustomize and helm. Scenario 2:
    // a manual scale-up of plain counts as a restore request too.
    let requested = Instant::now();
    full::request_restore(&client, "kustomize").await;
    full::request_restore(&client, "helm").await;
    Api::<Deployment>::namespaced(client.clone(), "plain")
        .patch(
            NAME,
            &PatchParams::default(),
            &Patch::Merge(&json!({ "spec": { "replicas": 2 } })),
        )
        .await
        .unwrap();
    for ns in FIXTURES {
        until(&client, ns, RESTORE_WITHIN, "restored", is_restored).await;
    }
    let took = requested.elapsed();

    // Leave full mode before the reaper sees the fixtures crash again.
    drop(full_mode);

    // SC-003: no difference from before the reap.
    let (after, hr_after) = snapshot(&client).await;
    for ns in FIXTURES {
        assert_eq!(after[ns], before[ns], "{ns}/{NAME} differs after restore");
    }
    assert_eq!(hr_after, hr_before, "helm/{NAME} HelmRelease differs");
    assert!(
        took < RESTORE_WITHIN,
        "restore took {took:?}; the watch should start a cycle at once"
    );
}
