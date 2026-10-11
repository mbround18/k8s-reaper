//! US1 on the dev cluster (quickstart §1–§2, SC-001). Opt-in:
//!
//! ```sh
//! make dev-kind image dev-load dev-run KUBE_CONTEXT=kind-...  # or: make dev-cluster && make image dev-load dev-run
//! make test-integration STORY=us1_report KUBE_CONTEXT=kind-...
//! ```
//!
//! Needs the dev fixtures (`deploy/dev/fixtures/`) and the reaper running in
//! report mode with the dev ReaperConfig. Reads only; it changes nothing.

mod common;

use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use kube::{Api, Client, Resource};
use reaper_core::record::{PLAN, STATE_LABEL};
use serde_json::Value;

/// Dev thresholds are 5m crash-looping and 3 restarts; allow for pod start-up
/// and a few 15 s cycles on top.
const DEADLINE: Duration = Duration::from_mins(12);
const POLL: Duration = Duration::from_secs(15);

fn state<K: Resource>(obj: &K) -> Option<String> {
    obj.meta().labels.as_ref()?.get(STATE_LABEL).cloned()
}

fn plan<K: Resource>(obj: &K) -> Option<Value> {
    let raw = obj.meta().annotations.as_ref()?.get(PLAN)?;
    Some(serde_json::from_str(raw).expect("plan annotation is JSON"))
}

async fn deployment(client: &Client, ns: &str, name: &str) -> Deployment {
    Api::namespaced(client.clone(), ns).get(name).await.unwrap()
}

#[tokio::test]
#[ignore = "needs the k3d dev cluster with the reaper running (make dev-cluster dev-run)"]
async fn report_mode_publishes_plans_and_changes_nothing() {
    let client = common::dev_client().await;

    let replicas_before = deployment(&client, "plain", "crashloop")
        .await
        .spec
        .unwrap()
        .replicas;

    // Scenarios 1–2: observed, then candidate with a published plan.
    let started = Instant::now();
    let plain = loop {
        let d = deployment(&client, "plain", "crashloop").await;
        if state(&d).as_deref() == Some("candidate") {
            break d;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "plain/crashloop never became a candidate (state {:?})",
            state(&d)
        );
        tokio::time::sleep(POLL).await;
    };
    let p = plan(&plain).expect("candidate carries a plan");
    assert!(
        p.get("skip").is_none(),
        "plain/crashloop should be reapable: {p}"
    );
    // contracts/annotations.md: a reapable plan is `{"actions": [...]}`; only
    // a skipped one carries `would`.
    assert!(
        p["actions"].as_array().is_some_and(|a| !a.is_empty()),
        "plan lists what full mode would do: {p}"
    );

    // Scenario 3: DaemonSets cannot be scaled to zero.
    let ds: DaemonSet = Api::namespaced(client.clone(), "ds")
        .get("crashloop")
        .await
        .unwrap();
    assert_eq!(state(&ds).as_deref(), Some("candidate"));
    assert_eq!(plan(&ds).unwrap()["skip"], "CannotScaleToZero");

    // Scenario 4: a workload with an available replica is never acted on.
    let partial = deployment(&client, "partial", "rollout").await;
    if let Some(p) = plan(&partial) {
        assert_eq!(p["skip"], "PartiallyAvailable");
    }

    // Scenario 5: an opted-out workload is left alone entirely.
    let optout = deployment(&client, "optout", "crashloop").await;
    assert_eq!(state(&optout), None);
    assert_eq!(plan(&optout), None);

    // SC-001: report mode never changes replicas, over a few more cycles.
    tokio::time::sleep(POLL * 4).await;
    let after = deployment(&client, "plain", "crashloop").await;
    assert_eq!(after.spec.as_ref().unwrap().replicas, replicas_before);
    assert_eq!(state(&after).as_deref(), Some("candidate"));
}
