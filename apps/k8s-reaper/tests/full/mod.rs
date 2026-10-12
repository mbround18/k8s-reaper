//! Shared by the full-mode dev-cluster stories (US2, US3, US5, US9): switching the
//! dev ReaperConfig to full mode and back, and waiting on the fixtures.

// Each story compiles this module on its own and uses only part of it.
#![allow(dead_code)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use k8s_openapi::api::apps::v1::Deployment;
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams};
use kube::{Api, Client, Resource};
use reaper_core::record::{ORIGINAL_REPLICAS, RESTORE, STATE_LABEL};
use serde_json::{Value, json};

use crate::common;

/// The reapable fixtures, one per management style.
pub const FIXTURES: [&str; 3] = ["plain", "kustomize", "helm"];
/// Every fixture is a Deployment of this name.
pub const NAME: &str = "crashloop";

/// Crash-looping 5 minutes from a fresh observation, plus pod start-up and
/// a few cycles.
pub const DEADLINE: Duration = Duration::from_mins(12);
pub const POLL: Duration = Duration::from_secs(5);

pub fn state<K: Resource>(obj: &K) -> Option<String> {
    obj.meta().labels.as_ref()?.get(STATE_LABEL).cloned()
}

pub fn annotation<K: Resource>(obj: &K, key: &str) -> Option<String> {
    obj.meta().annotations.as_ref()?.get(key).cloned()
}

pub fn replicas(d: &Deployment) -> Option<i32> {
    d.spec.as_ref()?.replicas
}

pub fn dynamic(group: &str, version: &str, kind: &str) -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind))
}

pub fn helm_releases(client: &Client) -> Api<DynamicObject> {
    let ar = dynamic("helm.toolkit.fluxcd.io", "v2", "HelmRelease");
    Api::namespaced_with(client.clone(), "helm", &ar)
}

pub fn suspended(hr: &DynamicObject) -> bool {
    hr.data["spec"]["suspend"].as_bool().unwrap_or(false)
}

pub const REQUESTED_AT: &str = "reconcile.fluxcd.io/requestedAt";

pub fn kustomizations(client: &Client) -> Api<DynamicObject> {
    let ar = dynamic("kustomize.toolkit.fluxcd.io", "v1", "Kustomization");
    Api::namespaced_with(client.clone(), "flux-system", &ar)
}

pub async fn merge(api: &Api<DynamicObject>, name: &str, patch: serde_json::Value) {
    api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .unwrap();
}

/// `flux reconcile` without the CLI: a new `requestedAt`, then, for the
/// Kustomization, wait until the controller has handled it. A suspended
/// HelmRelease never handles it, which is the point of the hold.
pub async fn force_reconcile(client: &Client) {
    let token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string();
    let patch = json!({ "metadata": { "annotations": { REQUESTED_AT: token } } });
    let ks = kustomizations(client);
    merge(&ks, "dev-kustomize", patch.clone()).await;
    merge(&helm_releases(client), NAME, patch).await;

    let started = Instant::now();
    loop {
        let k = ks.get("dev-kustomize").await.unwrap();
        if k.data["status"]["lastHandledReconcileAt"] == token.as_str() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_mins(3),
            "dev-kustomize never handled the reconcile request: {}",
            k.data["status"]
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    // Give helm-controller the same moment to act, if it were going to.
    tokio::time::sleep(Duration::from_secs(5)).await;
}

/// Polls `ns/crashloop` until `done` holds, failing after `deadline`.
pub async fn until(
    client: &Client,
    ns: &str,
    deadline: Duration,
    what: &str,
    done: impl Fn(&Deployment) -> bool,
) -> Deployment {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let started = Instant::now();
    loop {
        let d = api.get(NAME).await.unwrap();
        if done(&d) {
            return d;
        }
        assert!(
            started.elapsed() < deadline,
            "{ns}/{NAME} never became {what} (state {:?}, replicas {:?})",
            state(&d),
            replicas(&d)
        );
        tokio::time::sleep(POLL).await;
    }
}

pub fn is_reaped(d: &Deployment) -> bool {
    state(d).as_deref() == Some("reaped") && replicas(d) == Some(0)
}

/// Asks for a restore the way a person would (contracts/annotations.md).
pub async fn request_restore(client: &Client, ns: &str) {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let patch = json!({ "metadata": { "annotations": { RESTORE: "now" } } });
    api.patch(NAME, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .unwrap();
}

/// Puts every fixture that is still reaped or reaping back, so a story starts
/// from the fixtures as deployed.
pub async fn restore_leftovers(client: &Client) {
    let api_for = |ns| Api::<Deployment>::namespaced(client.clone(), ns);
    for ns in FIXTURES {
        let d = api_for(ns).get(NAME).await.unwrap();
        if annotation(&d, ORIGINAL_REPLICAS).is_some() {
            request_restore(client, ns).await;
            until(client, ns, Duration::from_mins(2), "restored", |d| {
                annotation(d, ORIGINAL_REPLICAS).is_none()
            })
            .await;
        }
    }
}

pub fn reaper_configs(client: &Client) -> Api<DynamicObject> {
    let ar = dynamic("reaper.boop.ninja", "v1alpha1", "ReaperConfig");
    Api::all_with(client.clone(), &ar)
}

pub async fn patch_config(client: &Client, spec: &Value) {
    reaper_configs(client)
        .patch(
            "default",
            &PatchParams::default(),
            &Patch::Merge(&json!({ "spec": spec })),
        )
        .await
        .unwrap();
}

/// The dev ReaperConfig in full mode, scoped to the fixtures these stories
/// own, until dropped. Dropping it puts the saved spec back, even when the
/// story panicked.
pub struct FullMode {
    saved: Value,
}

impl FullMode {
    pub async fn enter(client: &Client, cycle_interval: &str) -> Self {
        Self::enter_with(
            client,
            &json!({
                "cycleInterval": cycle_interval,
                "scope": { "includeNamespaces": ["plain", "kustomize", "helm", "partial"] },
                "limits": { "maxReapsPerCycle": FIXTURES.len() },
                // CronJobs are US5's; keep them out of the Deployment stories.
                "cronJobs": { "enabled": false },
            }),
        )
        .await
    }

    /// Full mode with `spec` merged over the saved spec.
    pub async fn enter_with(client: &Client, spec: &Value) -> Self {
        Self::enter_in(client, "full", spec).await
    }

    /// `mode` with `spec` merged over the saved spec, for a story that
    /// starts in report mode and switches to full with [`set_mode`].
    pub async fn enter_in(client: &Client, mode: &str, spec: &Value) -> Self {
        let config = reaper_configs(client).get("default").await.unwrap();
        let saved = config.data["spec"].clone();
        assert_eq!(
            saved["mode"], "report",
            "the dev ReaperConfig should be in report mode before a story"
        );
        let mut spec = spec.clone();
        spec["mode"] = json!(mode);
        patch_config(client, &spec).await;
        Self { saved }
    }
}

pub async fn set_mode(client: &Client, mode: &str) {
    patch_config(client, &json!({ "mode": mode })).await;
}

impl Drop for FullMode {
    fn drop(&mut self) {
        // The test's runtime is busy running this drop; use a fresh one.
        let saved = self.saved.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let client = common::dev_client().await;
                    patch_config(&client, &saved).await;
                });
        })
        .join()
        .expect("putting the dev ReaperConfig back");
    }
}
