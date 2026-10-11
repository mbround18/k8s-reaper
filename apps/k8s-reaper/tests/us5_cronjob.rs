//! US5 on the dev cluster (quickstart §5, SC-008). Opt-in:
//!
//! ```sh
//! make dev-kind dev-run KUBE_CONTEXT=kind-...   # or make dev-cluster dev-run
//! make test-integration STORY=us5_cronjob KUBE_CONTEXT=kind-...
//! ```
//!
//! Switches the dev ReaperConfig to full mode, scoped to the `cron` and
//! `kustomize` namespaces with crash-loop thresholds no Deployment reaches, so
//! only CronJobs are reaped. Waits for `cron/failing` and the Flux-managed
//! `kustomize/failing` to be suspended while `cron/flaky` never is, forces
//! three reconciles on `flux-system/dev-kustomize`, then restores both and
//! puts the ReaperConfig back.

mod common;
mod full;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use full::{FullMode, POLL, annotation, force_reconcile, state};
use k8s_openapi::api::batch::v1::CronJob;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use reaper_core::Timestamp;
use reaper_core::record::{FAILED_RUNS, FAILING_SINCE, FLUX_RECONCILE, RESTORE};
use serde_json::json;

const FAILING_DURATION: Duration = Duration::from_mins(10);
const MIN_FAILED_RUNS: u32 = 3;
/// A fresh streak needs the failing duration, plus Jobs and a few cycles.
const DEADLINE: Duration = Duration::from_mins(15);
/// SC-008: suspended within 2 cycles of qualifying; a poll of slack.
const SUSPEND_WITHIN: Duration = Duration::from_secs(2 * 15 + 10);
/// SC-008: one restore action is done within 1 cycle.
const RESTORE_WITHIN: Duration = Duration::from_secs(15 + 10);

const REAPED: [(&str, &str); 2] = [("cron", "failing"), ("kustomize", "failing")];
const FLAKY: (&str, &str) = ("cron", "flaky");

fn api(client: &Client, ns: &str) -> Api<CronJob> {
    Api::namespaced(client.clone(), ns)
}

async fn get(client: &Client, (ns, name): (&str, &str)) -> CronJob {
    api(client, ns).get(name).await.unwrap()
}

fn suspended(c: &CronJob) -> bool {
    c.spec.suspend == Some(true)
}

fn is_reaped(c: &CronJob) -> bool {
    suspended(c) && state(c).as_deref() == Some("reaped")
}

/// When the reaper's own record says the CronJob met both thresholds.
fn qualified(c: &CronJob, now: SystemTime) -> bool {
    let since = annotation(c, FAILING_SINCE).and_then(|s| Timestamp::parse_rfc3339(&s).ok());
    let runs = annotation(c, FAILED_RUNS).and_then(|s| s.parse::<u32>().ok());
    let now = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let now = i64::try_from(now).unwrap();
    let long_enough =
        since.is_some_and(|s| now - s.0 >= i64::try_from(FAILING_DURATION.as_secs()).unwrap());
    long_enough && runs.is_some_and(|r| r >= MIN_FAILED_RUNS)
}

async fn request_restore(client: &Client, (ns, name): (&str, &str)) {
    let patch = json!({ "metadata": { "annotations": { RESTORE: "now" } } });
    api(client, ns)
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .unwrap();
}

/// Waits until `target` is no longer suspended nor reaped.
async fn until_resumed(client: &Client, target: (&str, &str), within: Duration) {
    let started = Instant::now();
    loop {
        let c = get(client, target).await;
        if !suspended(&c) && state(&c).as_deref() != Some("reaped") {
            return;
        }
        assert!(
            started.elapsed() < within,
            "{}/{} not resumed after {within:?} (state {:?})",
            target.0,
            target.1,
            state(&c)
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Puts back any CronJob a previous run left reaped.
async fn restore_leftovers(client: &Client) {
    for target in REAPED {
        if is_reaped(&get(client, target).await) {
            request_restore(client, target).await;
            until_resumed(client, target, Duration::from_mins(2)).await;
        }
    }
}

#[tokio::test]
#[ignore = "needs the dev cluster with Flux and the reaper running (make dev-kind dev-run)"]
async fn a_cron_job_that_keeps_failing_is_suspended_and_restored() {
    let client = common::dev_client().await;
    restore_leftovers(&client).await;

    let full_mode = FullMode::enter_with(
        &client,
        &json!({
            "cycleInterval": "15s",
            "scope": { "includeNamespaces": ["cron", "kustomize"] },
            "detection": { "crashLoopDuration": "7d", "minRestarts": 10_000 },
            "cronJobs": {
                "enabled": true,
                "failingDuration": "10m",
                "minFailedRuns": MIN_FAILED_RUNS,
            },
            "limits": { "maxReapsPerCycle": REAPED.len() },
        }),
    )
    .await;

    // Scenario 1: each failing CronJob is suspended within 2 cycles of
    // qualifying. Scenario 2: the flaky one never is.
    let started = Instant::now();
    let mut qualified_at: [Option<Instant>; 2] = [None; 2];
    let mut reaped_at: [Option<Instant>; 2] = [None; 2];
    while reaped_at.iter().any(Option::is_none) {
        assert!(
            started.elapsed() < DEADLINE,
            "not every failing CronJob was suspended: {reaped_at:?}"
        );
        for (i, target) in REAPED.into_iter().enumerate() {
            let c = get(&client, target).await;
            if qualified_at[i].is_none() && qualified(&c, SystemTime::now()) {
                qualified_at[i] = Some(Instant::now());
            }
            if reaped_at[i].is_none() && is_reaped(&c) {
                reaped_at[i] = Some(Instant::now());
            }
        }
        let flaky = get(&client, FLAKY).await;
        assert!(
            !suspended(&flaky) && state(&flaky).as_deref() != Some("reaped"),
            "cron/flaky was suspended though it succeeds every other run"
        );
        tokio::time::sleep(POLL).await;
    }
    for (i, (ns, name)) in REAPED.into_iter().enumerate() {
        // Already qualified when full mode began: counts from the start.
        let from = qualified_at[i].unwrap_or(started);
        let took = reaped_at[i].unwrap().saturating_duration_since(from);
        assert!(
            took < SUSPEND_WITHIN,
            "{ns}/{name} took {took:?} to be suspended after qualifying"
        );
    }

    // Flux holds: reconcile disabled, and three forced reconciles take
    // nothing back.
    let ks = get(&client, REAPED[1]).await;
    assert_eq!(annotation(&ks, FLUX_RECONCILE).as_deref(), Some("disabled"));
    for round in 1..=3 {
        force_reconcile(&client).await;
        let c = get(&client, REAPED[1]).await;
        assert!(
            is_reaped(&c),
            "reconcile {round}: kustomize/failing resumed"
        );
    }

    // Scenario 4: one restore action resumes it within 1 cycle.
    for target in REAPED {
        request_restore(&client, target).await;
    }
    for target in REAPED {
        until_resumed(&client, target, RESTORE_WITHIN).await;
    }
    let ks = get(&client, REAPED[1]).await;
    assert_ne!(annotation(&ks, FLUX_RECONCILE).as_deref(), Some("disabled"));

    // Leave full mode before the reaper sees them fail long enough again.
    drop(full_mode);
}
