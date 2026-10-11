//! US4 on the dev cluster (quickstart §5, FR-010, FR-024, FR-026, SC-005).
//! Opt-in:
//!
//! ```sh
//! make dev-kind image dev-load dev-run KUBE_CONTEXT=kind-...  # or: make dev-cluster && make image dev-load dev-run
//! make test-integration STORY=us4_config KUBE_CONTEXT=kind-...
//! ```
//!
//! Labels the `kustomize` namespace `mode=report` and `helm/crashloop`
//! `mode=off`, starts a crash-looping Deployment in the reaper's own
//! namespace, then switches the running reaper to full mode, scoped to the
//! fixtures and its own namespace. Puts the config, the labels and the extra
//! Deployment back afterwards and restores the reaped fixture.

mod common;
mod full;

use std::time::{Duration, Instant};

use full::{FullMode, NAME, annotation, is_reaped, replicas, state, until};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::jiff;
use kube::api::{DeleteParams, DynamicObject, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};
use reaper_core::record::{FLUX_RECONCILE, MODE_KEY, ORIGINAL_REPLICAS};
use serde_json::{Value, json};

/// The reaper's own namespace in deploy/dev/reaper.yaml.
const OWN: &str = "k8s-reaper";

/// Sets or, with `None`, removes the mode label on the `kustomize` namespace.
async fn label_namespace(client: &Client, value: Option<&str>) {
    Api::<Namespace>::all(client.clone())
        .patch("kustomize", &PatchParams::default(), &mode_label(value))
        .await
        .unwrap();
}

/// Sets or, with `None`, removes the mode label on `helm/crashloop`.
async fn label_workload(client: &Client, value: Option<&str>) {
    Api::<Deployment>::namespaced(client.clone(), "helm")
        .patch(NAME, &PatchParams::default(), &mode_label(value))
        .await
        .unwrap();
}

fn mode_label(value: Option<&str>) -> Patch<Value> {
    Patch::Merge(json!({ "metadata": { "labels": { MODE_KEY: value } } }))
}

/// A copy of the plain fixture in the reaper's own namespace.
fn own_crashloop() -> Deployment {
    serde_json::from_value(json!({
        "metadata": { "name": NAME, "namespace": OWN },
        "spec": {
            "replicas": 1,
            "selector": { "matchLabels": { "app": NAME } },
            "template": {
                "metadata": { "labels": { "app": NAME } },
                "spec": {
                    "terminationGracePeriodSeconds": 1,
                    "containers": [{
                        "name": "app",
                        "image": "busybox:1.36.1",
                        "command": ["false"],
                        "resources": {
                            "requests": { "cpu": "5m", "memory": "8Mi" },
                            "limits": { "memory": "16Mi" },
                        },
                    }],
                },
            },
        },
    }))
    .unwrap()
}

/// Labels and the extra Deployment gone, whatever an earlier run left.
async fn clean(client: &Client) {
    label_namespace(client, None).await;
    label_workload(client, None).await;
    let own: Api<Deployment> = Api::namespaced(client.clone(), OWN);
    if own.get_opt(NAME).await.unwrap().is_some() {
        own.delete(NAME, &DeleteParams::default()).await.unwrap();
    }
    // Gone before the story creates it again.
    while own.get_opt(NAME).await.unwrap().is_some() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Polls the dev ReaperConfig until `done` holds for it.
async fn config_until(client: &Client, what: &str, done: impl Fn(&DynamicObject) -> bool) {
    let api = full::reaper_configs(client);
    let started = Instant::now();
    loop {
        let c = api.get("default").await.unwrap();
        if done(&c) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_mins(2),
            "the ReaperConfig never {what}: {}",
            c.data["status"]
        );
        tokio::time::sleep(full::POLL).await;
    }
}

fn generation(c: &DynamicObject) -> i64 {
    c.metadata.generation.unwrap()
}

fn valid(c: &DynamicObject) -> Option<&Value> {
    c.data["status"]["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["type"] == "Valid")
}

/// The reaper read this generation and made it active.
fn applied(c: &DynamicObject) -> bool {
    let g = generation(c);
    c.data["status"]["activeGeneration"] == g
        && valid(c).is_some_and(|v| v["status"] == "True" && v["observedGeneration"] == g)
}

/// An Event on the ReaperConfig with this reason about `generation`, emitted
/// or repeated at or after `since` (G18: a new note is a new Event).
async fn config_event(
    client: &Client,
    reason: &str,
    generation: i64,
    since: jiff::Timestamp,
) -> bool {
    let needle = format!("generation {generation} ");
    // Events about cluster-scoped objects live in `default`.
    let events: Api<Event> = Api::namespaced(client.clone(), "default");
    events
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .iter()
        .any(|e| {
            let last = e
                .series
                .as_ref()
                .map(|s| s.last_observed_time.0)
                .or(e.event_time.as_ref().map(|t| t.0));
            e.reason.as_deref() == Some(reason)
                && e.regarding.as_ref().and_then(|r| r.kind.as_deref()) == Some("ReaperConfig")
                && last.is_some_and(|t| t >= since)
                && e.note.as_deref().is_some_and(|n| n.starts_with(&needle))
        })
}

/// A moment just before a config change, allowing for the API server
/// truncating Event times to microseconds.
fn before() -> jiff::Timestamp {
    jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1)
}

#[tokio::test]
#[ignore = "needs the dev cluster with Flux and the reaper running (make dev-kind dev-run)"]
async fn config_changes_apply_without_a_restart() {
    let client = common::dev_client().await;
    clean(&client).await;
    full::restore_leftovers(&client).await;

    // Created first, so it has been crash-looping at least as long as the
    // fixtures by the time they are reaped.
    let own: Api<Deployment> = Api::namespaced(client.clone(), OWN);
    own.create(&PostParams::default(), &own_crashloop())
        .await
        .unwrap();
    label_namespace(&client, Some("report")).await;
    label_workload(&client, Some("off")).await;

    let since = before();
    let full_mode = FullMode::enter(&client, "15s").await;
    full::patch_config(
        &client,
        &json!({ "scope": { "includeNamespaces": ["plain", "kustomize", "helm", OWN] } }),
    )
    .await;

    // Scenario 1: the running reaper picks the new generation up.
    config_until(&client, "applied the full-mode generation", applied).await;
    let active = generation(&full::reaper_configs(&client).get("default").await.unwrap());
    assert!(config_event(&client, "ConfigApplied", active, since).await);

    // Scenario 4: an invalid generation is reported and the last valid one
    // stays active.
    let since = before();
    full::patch_config(&client, &json!({ "cycleInterval": "5s" })).await;
    config_until(&client, "reported the invalid generation", |c| {
        valid(c).is_some_and(|v| v["status"] == "False" && v["observedGeneration"] == generation(c))
            && c.data["status"]["activeGeneration"] == active
    })
    .await;
    let invalid = generation(&full::reaper_configs(&client).get("default").await.unwrap());
    assert!(config_event(&client, "ConfigInvalid", invalid, since).await);
    full::patch_config(&client, &json!({ "cycleInterval": "15s" })).await;
    config_until(&client, "applied the fixed generation", applied).await;

    // Scenario 1, continued: full mode reaps without a restart.
    until(&client, "plain", full::DEADLINE, "reaped", is_reaped).await;
    // A few more cycles for anything else that was going to be reaped.
    tokio::time::sleep(Duration::from_mins(1)).await;

    // Scenario 2: `mode=report` on the namespace leaves its workload alone.
    let k: Deployment = Api::namespaced(client.clone(), "kustomize")
        .get(NAME)
        .await
        .unwrap();
    assert_eq!(replicas(&k), Some(2), "kustomize/{NAME} was scaled");
    assert_eq!(annotation(&k, ORIGINAL_REPLICAS), None);
    assert_eq!(annotation(&k, FLUX_RECONCILE), None);
    assert!(
        !matches!(state(&k).as_deref(), Some("reaping" | "reaped")),
        "kustomize/{NAME} was reaped"
    );

    // Scenario 3: `mode=off` on the workload: no state, no hold.
    let h: Deployment = Api::namespaced(client.clone(), "helm")
        .get(NAME)
        .await
        .unwrap();
    assert_eq!(replicas(&h), Some(2), "helm/{NAME} was scaled");
    assert_eq!(state(&h), None, "helm/{NAME} carries a state");
    let hr = full::helm_releases(&client).get(NAME).await.unwrap();
    assert!(!full::suspended(&hr), "helm/{NAME} was suspended");

    // Scenario 5 (SC-005): the reaper's own namespace is never touched.
    let o = own.get(NAME).await.unwrap();
    assert_eq!(replicas(&o), Some(1), "{OWN}/{NAME} was scaled");
    assert_eq!(state(&o), None, "{OWN}/{NAME} carries a state");

    drop(full_mode);
    clean(&client).await;
    full::restore_leftovers(&client).await;
}
