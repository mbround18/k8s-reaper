//! Spec 003 US1 on the dev cluster (quickstart §2, SC-001, SC-006). Opt-in:
//!
//! ```sh
//! make dev-kind image dev-load dev-run KUBE_CONTEXT=kind-...  # or: make dev-cluster && make image dev-load dev-run
//! make test-integration STORY=us9_flaky_cronjob KUBE_CONTEXT=kind-...
//! ```
//!
//! Creates `cron-rate/mostly-failing`, a minutely CronJob that fails unless
//! the minute is divisible by 5: four failures in every five runs, and never
//! a streak long enough for the streak rule. Its own namespace keeps the
//! `cron` fixtures out of the story's scope. Scoped to it, in report mode with
//! `windowRuns: 5` and `threshold: 80%`, the reaper records each run once and
//! plans the reap on the rate; in full mode it suspends the CronJob. The
//! story restores and deletes the CronJob and puts the ReaperConfig back.

mod common;
mod full;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use full::{FullMode, POLL, annotation, set_mode, state};
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::jiff;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, PropagationPolicy};
use kube::{Api, Client};
use reaper_core::record::{RESTORE, RUN_HISTORY, RunHistory};
use serde_json::json;

const NS: &str = "cron-rate";
const NAME: &str = "mostly-failing";
const WINDOW_RUNS: usize = 5;
/// The history must span `failingDuration` (10m); then a minute or two for
/// the newest run to be a failure and the cycle to see it.
const DEADLINE: Duration = Duration::from_mins(16);
/// Two 15s cycles, plus time for the writes and the Events to land.
const WITHIN: Duration = Duration::from_secs(2 * 15 + 45);
/// The reason's rate clause (contracts/annotations.md); one run in five
/// succeeds, so the window holds 4 failures.
const RATE_CLAUSE: &str = "4 of last 5 runs failed";

fn cron_jobs(client: &Client) -> Api<CronJob> {
    Api::namespaced(client.clone(), NS)
}

async fn get(client: &Client) -> CronJob {
    cron_jobs(client).get(NAME).await.unwrap()
}

fn suspended(c: &CronJob) -> bool {
    c.spec.suspend == Some(true)
}

fn history(c: &CronJob) -> Option<RunHistory> {
    let raw = annotation(c, RUN_HISTORY)?;
    Some(RunHistory::decode(&raw).unwrap_or_else(|| panic!("unreadable run-history: {raw}")))
}

/// Scenario 3: runs are ordered, no Job is recorded twice, and every
/// recorded name is one of this CronJob's Jobs.
fn assert_well_formed(h: &RunHistory, seen: &mut BTreeSet<String>) {
    assert!(
        h.runs.len() <= WINDOW_RUNS,
        "history over the window: {h:?}"
    );
    assert!(
        h.runs.windows(2).all(|w| w[0].at <= w[1].at),
        "history out of order: {h:?}"
    );
    let named: Vec<_> = h.runs.iter().filter_map(|r| r.job.as_deref()).collect();
    let unique: BTreeSet<_> = named.iter().collect();
    assert_eq!(unique.len(), named.len(), "a Job recorded twice: {h:?}");
    for job in named {
        assert!(job.starts_with(NAME), "not one of {NAME}'s Jobs: {job}");
        seen.insert(job.to_owned());
    }
}

/// A moment just before a change, allowing for the API server truncating
/// Event times to microseconds.
fn before() -> jiff::Timestamp {
    jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1)
}

/// An Event about the CronJob with this reason and a note containing
/// `needle`, emitted or repeated at or after `since`.
async fn event(client: &Client, reason: &str, needle: &str, since: jiff::Timestamp) -> bool {
    let events: Api<Event> = Api::namespaced(client.clone(), NS);
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
                && regarding.and_then(|r| r.kind.as_deref()) == Some("CronJob")
                && regarding.and_then(|r| r.name.as_deref()) == Some(NAME)
                && last.is_some_and(|t| t >= since)
                && e.note.as_deref().is_some_and(|n| n.contains(needle))
        })
}

async fn expect_event(client: &Client, reason: &str, needle: &str, since: jiff::Timestamp) {
    let started = Instant::now();
    while !event(client, reason, needle, since).await {
        assert!(
            started.elapsed() < WITHIN,
            "no {reason} Event with `{needle}` on {NS}/{NAME} within {WITHIN:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// The namespace, kept between runs, and no CronJob left from a previous one.
async fn fresh_namespace(client: &Client) {
    let namespaces: Api<Namespace> = Api::all(client.clone());
    if namespaces.get_opt(NS).await.unwrap().is_none() {
        let ns = serde_json::from_value(json!({ "metadata": { "name": NS } })).unwrap();
        namespaces
            .create(&PostParams::default(), &ns)
            .await
            .unwrap();
    }
    delete_cron_job(client).await;
}

async fn delete_cron_job(client: &Client) {
    let api = cron_jobs(client);
    let params = DeleteParams {
        propagation_policy: Some(PropagationPolicy::Foreground),
        ..DeleteParams::default()
    };
    if api.get_opt(NAME).await.unwrap().is_none() {
        return;
    }
    api.delete(NAME, &params).await.unwrap();
    let started = Instant::now();
    while api.get_opt(NAME).await.unwrap().is_some() {
        assert!(
            started.elapsed() < Duration::from_mins(2),
            "{NS}/{NAME} not deleted"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn cron_job() -> CronJob {
    serde_json::from_value(json!({
        "metadata": { "name": NAME, "namespace": NS },
        "spec": {
            "schedule": "* * * * *",
            "concurrencyPolicy": "Forbid",
            "successfulJobsHistoryLimit": 1,
            "failedJobsHistoryLimit": 1,
            "jobTemplate": { "spec": {
                "backoffLimit": 0,
                // Successes and failures alike are gone before long, so the
                // reaper has to keep its own record.
                "ttlSecondsAfterFinished": 30,
                "template": { "spec": {
                    "restartPolicy": "Never",
                    "terminationGracePeriodSeconds": 1,
                    "containers": [{
                        "name": "app",
                        "image": "busybox:1.36.1",
                        "command": ["sh", "-c", "test $(( $(date +%M) % 5 )) -eq 0"],
                        "resources": {
                            "requests": { "cpu": "5m", "memory": "8Mi" },
                            "limits": { "memory": "16Mi" }
                        }
                    }]
                }}
            }}
        }
    }))
    .unwrap()
}

#[tokio::test]
#[ignore = "needs the dev cluster with the reaper running (make dev-kind dev-run)"]
async fn a_cron_job_that_fails_most_runs_is_suspended() {
    let client = common::dev_client().await;
    fresh_namespace(&client).await;
    let config = FullMode::enter_in(
        &client,
        "report",
        &json!({
            "cycleInterval": "15s",
            "scope": { "includeNamespaces": [NS] },
            "detection": { "crashLoopDuration": "168h", "minRestarts": 10_000 },
            "cronJobs": {
                "enabled": true,
                "failingDuration": "10m",
                "minFailedRuns": 3,
                "failureRate": { "enabled": true, "windowRuns": WINDOW_RUNS, "threshold": "80%" },
            },
            "limits": { "maxReapsPerCycle": 1 },
        }),
    )
    .await;
    let since = before();
    cron_jobs(&client)
        .create(&PostParams::default(), &cron_job())
        .await
        .unwrap();

    // Scenarios 1 and 3: each run is recorded once, and the rate makes it a
    // candidate in report mode without suspending it.
    let started = Instant::now();
    let mut seen = BTreeSet::new();
    let c = loop {
        let c = get(&client).await;
        if let Some(h) = history(&c) {
            assert_well_formed(&h, &mut seen);
        }
        if state(&c).as_deref() == Some("candidate") {
            break c;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "{NS}/{NAME} never became a candidate (state {:?}, history {:?})",
            state(&c),
            annotation(&c, RUN_HISTORY)
        );
        tokio::time::sleep(POLL).await;
    };
    assert!(!suspended(&c), "report mode suspended {NS}/{NAME}");
    let h = history(&c).expect("a candidate on the rate carries its history");
    assert_eq!(h.runs.len(), WINDOW_RUNS, "window not full: {h:?}");
    assert_eq!(h.failed(), 4, "window: {h:?}");
    assert!(!h.runs.last().unwrap().ok, "newest run succeeded: {h:?}");
    assert!(
        seen.len() >= WINDOW_RUNS - 1,
        "too few Jobs recorded: {seen:?}"
    );
    expect_event(&client, "ReapPlanned", RATE_CLAUSE, since).await;

    // Scenario 2 (SC-006): full mode suspends it, naming the rate.
    let since = before();
    set_mode(&client, "full").await;
    let started = Instant::now();
    loop {
        let c = get(&client).await;
        if suspended(&c) && state(&c).as_deref() == Some("reaped") {
            break;
        }
        // The newest run may be the one success in five; the next failure
        // makes it a candidate again a minute later.
        assert!(
            started.elapsed() < Duration::from_mins(2) + WITHIN,
            "{NS}/{NAME} not suspended in full mode (state {:?})",
            state(&c)
        );
        tokio::time::sleep(POLL).await;
    }
    expect_event(&client, "Reaped", RATE_CLAUSE, since).await;

    // Put it back and leave nothing behind.
    let patch = json!({ "metadata": { "annotations": { RESTORE: "now" } } });
    cron_jobs(&client)
        .patch(NAME, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .unwrap();
    let started = Instant::now();
    while suspended(&get(&client).await) {
        assert!(started.elapsed() < WITHIN, "{NS}/{NAME} not resumed");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    drop(config);
    delete_cron_job(&client).await;
}
