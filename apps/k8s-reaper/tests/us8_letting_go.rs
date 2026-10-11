//! 002 US3 on the dev cluster (002 quickstart §5, FR-009, FR-010). Opt-in:
//!
//! ```sh
//! make image dev-load dev-run KUBE_CONTEXT=kind-dev
//! make test-integration STORY=us8_letting_go KUBE_CONTEXT=kind-dev
//! ```
//!
//! Switches the dev ReaperConfig to full mode and lets the reaper reap the
//! three crash-looping fixtures. Then:
//!
//! 1. resumes `helm/crashloop`'s HelmRelease the way `flux resume` does, and
//!    expects `Restored manual-resume` and `HelmReleaseReleased` within one
//!    cycle;
//! 2. adds `plain` and `kustomize` to `excludeNamespaces`, and expects
//!    `Restored opted-out` for both within one cycle, with the Kustomization
//!    reconcile annotation put back on `kustomize/crashloop` (the only
//!    fixture managed by a Kustomization), and nothing written to them
//!    afterwards.
//!
//! The dev ReaperConfig is put back when the story ends, even on a panic.

mod common;
mod full;

use std::time::{Duration, Instant};

use full::{FIXTURES, FullMode, NAME, annotation, is_reaped, replicas, state, until};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::jiff;
use kube::api::ListParams;
use kube::{Api, Client};
use reaper_core::record::{FLUX_RECONCILE, HELD_FOR, ORIGINAL_REPLICAS};
use serde_json::{Value, json};

/// The cycle interval while the story runs.
const CYCLE: Duration = Duration::from_mins(1);
/// One cycle, plus time for the writes and the Events to land.
const WITHIN: Duration = Duration::from_secs(90);

/// The namespaces the story takes out of scope.
const LEAVING: [&str; 2] = ["plain", "kustomize"];

/// A moment just before a change, allowing for the API server truncating
/// Event times to microseconds.
fn before() -> jiff::Timestamp {
    jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1)
}

/// An Event in `ns` about `kind` `ns/name`, with this reason and a note
/// starting `note`, emitted or repeated at or after `since`.
async fn event(
    client: &Client,
    (ns, kind, name): (&str, &str, &str),
    reason: &str,
    note: &str,
    since: jiff::Timestamp,
) -> bool {
    let events: Api<Event> = Api::namespaced(client.clone(), ns);
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
            let regarding = e.regarding.as_ref();
            e.reason.as_deref() == Some(reason)
                && regarding.and_then(|r| r.kind.as_deref()) == Some(kind)
                && regarding.and_then(|r| r.name.as_deref()) == Some(name)
                && last.is_some_and(|t| t >= since)
                && e.note.as_deref().is_some_and(|n| n.starts_with(note))
        })
}

/// Waits up to `WITHIN` for that Event.
async fn expect_event(
    client: &Client,
    about: (&str, &str, &str),
    reason: &str,
    note: &str,
    since: jiff::Timestamp,
) {
    let started = Instant::now();
    while !event(client, about, reason, note, since).await {
        assert!(
            started.elapsed() < WITHIN,
            "no {reason} Event with note `{note}…` on {about:?} within {WITHIN:?}"
        );
        tokio::time::sleep(full::POLL).await;
    }
}

/// Restored: the record is gone and the replicas are back.
fn is_restored(d: &Deployment) -> bool {
    annotation(d, ORIGINAL_REPLICAS).is_none() && state(d).is_none() && replicas(d) == Some(2)
}

#[tokio::test]
#[ignore = "needs the dev cluster with Flux and the reaper running (make dev-run)"]
async fn the_reaper_lets_go_of_what_it_no_longer_looks_at() {
    let client = common::dev_client().await;
    full::restore_leftovers(&client).await;
    let deployments = |ns| Api::<Deployment>::namespaced(client.clone(), ns);
    let reconcile_before = annotation(
        &deployments("kustomize").get(NAME).await.unwrap(),
        FLUX_RECONCILE,
    );

    let cycle = format!("{}s", CYCLE.as_secs());
    // Dropped last: puts the whole saved spec back, scope included.
    let _full_mode = FullMode::enter(&client, &cycle).await;
    for ns in FIXTURES {
        until(&client, ns, full::DEADLINE, "reaped", is_reaped).await;
    }
    let hrs = full::helm_releases(&client);
    let hr = hrs.get(NAME).await.unwrap();
    assert!(full::suspended(&hr) && annotation(&hr, HELD_FOR).is_some());

    // Scenario 1: `flux resume helmrelease crashloop -n helm`, without the CLI.
    let resumed = before();
    full::merge(&hrs, NAME, json!({ "spec": { "suspend": false } })).await;
    expect_event(
        &client,
        ("helm", "Deployment", NAME),
        "Restored",
        "manual-resume",
        resumed,
    )
    .await;
    expect_event(
        &client,
        ("helm", "HelmRelease", NAME),
        "HelmReleaseReleased",
        "",
        resumed,
    )
    .await;
    until(&client, "helm", WITHIN, "restored", is_restored).await;
    let hr = hrs.get(NAME).await.unwrap();
    assert_eq!(annotation(&hr, HELD_FOR), None, "held-for is still set");
    assert!(!full::suspended(&hr), "the reaper suspended it again");

    // Scenario 3: the namespaces leave scope.
    let config = full::reaper_configs(&client).get("default").await.unwrap();
    let mut exclude: Vec<Value> = config.data["spec"]["scope"]["excludeNamespaces"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    exclude.extend(LEAVING.map(Value::from));
    let excluded = before();
    full::patch_config(
        &client,
        &json!({ "scope": { "excludeNamespaces": exclude } }),
    )
    .await;
    for ns in LEAVING {
        expect_event(
            &client,
            (ns, "Deployment", NAME),
            "Restored",
            "opted-out",
            excluded,
        )
        .await;
        until(&client, ns, WITHIN, "restored", is_restored).await;
    }
    let kustomize = deployments("kustomize").get(NAME).await.unwrap();
    assert_eq!(
        annotation(&kustomize, FLUX_RECONCILE),
        reconcile_before,
        "the Kustomization reconcile annotation was not put back"
    );

    // Out of scope now: the next cycle observes nothing there, although
    // the fixtures still crash.
    let settled = before();
    tokio::time::sleep(CYCLE + Duration::from_secs(15)).await;
    for ns in LEAVING {
        let d = deployments(ns).get(NAME).await.unwrap();
        assert_eq!(state(&d), None, "{ns}/{NAME} was labelled out of scope");
        assert!(
            !event(
                &client,
                (ns, "Deployment", NAME),
                "CrashLoopObserved",
                "",
                settled
            )
            .await,
            "{ns}/{NAME} was observed out of scope"
        );
    }
}
