//! US2 on the dev cluster (quickstart §3, SC-002, research R4). Opt-in:
//!
//! ```sh
//! make dev-kind dev-run KUBE_CONTEXT=kind-...   # or make dev-cluster dev-run
//! make test-integration STORY=us2_reap KUBE_CONTEXT=kind-...
//! ```
//!
//! Switches the dev ReaperConfig to full mode, scoped to the plain, kustomize,
//! helm and partial fixtures, and puts it back afterwards. Forces reconciles
//! on the `flux-system/dev-kustomize` Kustomization and the `helm/crashloop`
//! HelmRelease, and briefly sets `force: true` on the Kustomization. Restores
//! the reaped fixtures at the end.

mod common;
mod full;

use full::{
    FIXTURES, FullMode, NAME, annotation, force_reconcile, is_reaped, kustomizations, merge,
    replicas, state, until,
};
use k8s_openapi::api::apps::v1::Deployment;

use kube::{Api, Client, ResourceExt};
use reaper_core::record::{
    FLUX_RECONCILE, HELD_FOR, HELMRELEASE, ORIGINAL_REPLICAS, PREVIOUS_RECONCILE, REAPED_AT,
    SUSPENDED_BY_REAPER,
};
use serde_json::json;

/// SC-002: every fixture still at 0 and still held.
async fn assert_held(client: &Client, uid: &str, round: &str) {
    for ns in FIXTURES {
        let d: Deployment = Api::namespaced(client.clone(), ns).get(NAME).await.unwrap();
        assert!(
            is_reaped(&d),
            "{round}: {ns}/{NAME} came back (state {:?}, replicas {:?})",
            state(&d),
            replicas(&d)
        );
    }
    let k: Deployment = Api::namespaced(client.clone(), "kustomize")
        .get(NAME)
        .await
        .unwrap();
    // R4: never pruned or recreated while reconcile is disabled.
    assert_eq!(
        k.uid().as_deref(),
        Some(uid),
        "{round}: kustomize/{NAME} was recreated"
    );
    assert_eq!(annotation(&k, FLUX_RECONCILE).as_deref(), Some("disabled"));
    let hr = full::helm_releases(client).get(NAME).await.unwrap();
    assert!(full::suspended(&hr), "{round}: helm/{NAME} was unsuspended");
}

#[tokio::test]
#[ignore = "needs the dev cluster with Flux and the reaper running (make dev-kind dev-run)"]
async fn full_mode_reaps_and_flux_does_not_undo_it() {
    let client = common::dev_client().await;
    full::restore_leftovers(&client).await;
    let partial_before = replicas(
        &Api::<Deployment>::namespaced(client.clone(), "partial")
            .get("rollout")
            .await
            .unwrap(),
    );

    let full_mode = FullMode::enter(&client, "15s").await;

    // Scenario 1: each fixture is reaped and carries its record.
    for ns in FIXTURES {
        let d = until(&client, ns, full::DEADLINE, "reaped", is_reaped).await;
        assert_eq!(annotation(&d, ORIGINAL_REPLICAS).as_deref(), Some("2"));
        assert!(annotation(&d, REAPED_AT).is_some(), "{ns}: no reaped-at");
    }

    // Scenario 2: kustomize reconcile is disabled, the previous value kept.
    let k: Deployment = Api::namespaced(client.clone(), "kustomize")
        .get(NAME)
        .await
        .unwrap();
    assert_eq!(annotation(&k, FLUX_RECONCILE).as_deref(), Some("disabled"));
    assert_eq!(
        annotation(&k, PREVIOUS_RECONCILE).as_deref(),
        Some("<none>")
    );
    let uid = k.uid().unwrap();

    // Scenario 3: the HelmRelease is suspended and holds the workload.
    let h: Deployment = Api::namespaced(client.clone(), "helm")
        .get(NAME)
        .await
        .unwrap();
    assert_eq!(
        annotation(&h, HELMRELEASE).as_deref(),
        Some("helm/crashloop")
    );
    let hr = full::helm_releases(&client).get(NAME).await.unwrap();
    assert!(full::suspended(&hr));
    assert_eq!(
        annotation(&hr, HELD_FOR).as_deref(),
        Some(r#"["Deployment/helm/crashloop"]"#)
    );
    assert_eq!(
        annotation(&hr, SUSPENDED_BY_REAPER).as_deref(),
        Some("true")
    );

    // SC-002: three forced reconciles take nothing back.
    for round in 1..=3 {
        force_reconcile(&client).await;
        assert_held(&client, &uid, &format!("reconcile {round}")).await;
    }

    // R4: `force: true` still honours the reconcile annotation. Put it back
    // before asserting so a failure does not leave the Kustomization forced.
    let ks = kustomizations(&client);
    let force_before = ks.get("dev-kustomize").await.unwrap().data["spec"]["force"].clone();
    merge(&ks, "dev-kustomize", json!({ "spec": { "force": true } })).await;
    let forced = tokio::spawn({
        let client = client.clone();
        async move {
            force_reconcile(&client).await;
            assert_held(&client, &uid, "forced reconcile").await;
        }
    })
    .await;
    merge(
        &ks,
        "dev-kustomize",
        json!({ "spec": { "force": force_before } }),
    )
    .await;
    if let Err(err) = forced {
        std::panic::resume_unwind(err.into_panic());
    }

    // Scenario 4: a PartiallyAvailable workload is untouched, after all of
    // those full-mode cycles.
    let partial: Deployment = Api::namespaced(client.clone(), "partial")
        .get("rollout")
        .await
        .unwrap();
    assert_eq!(replicas(&partial), partial_before);
    assert_eq!(annotation(&partial, ORIGINAL_REPLICAS), None);
    assert!(
        !matches!(state(&partial).as_deref(), Some("reaping" | "reaped")),
        "partial/rollout was reaped"
    );

    drop(full_mode);
    full::restore_leftovers(&client).await;
}
