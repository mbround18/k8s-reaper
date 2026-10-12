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
//! plans the reap on the rate, and per-CronJob annotations only raise the
//! window (quickstart §4). In full mode it suspends the CronJob; a restore
//! starts its history over (§2 step 6). Once every run succeeds, nothing is
//! left on it (§3). The story deletes the CronJob and puts the ReaperConfig
//! back.

mod common;
mod full;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use full::{FullMode, POLL, annotation, set_mode, state};
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use k8s_openapi::api::events::v1::Event;
use k8s_openapi::jiff;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, PropagationPolicy};
use kube::{Api, Client};
use reaper_core::record::{FAILED_RUNS, FAILING_SINCE, RESTORE, RUN_HISTORY, RunHistory};
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
/// Time for the CronJob's next run (or the next failure, one run in five
/// being a success) to land and be seen.
const NEXT_RUN: Duration = Duration::from_mins(2);
const FAILURE_WINDOW: &str = "reaper.boop.ninja/failure-window";
const FAILURE_RATE: &str = "reaper.boop.ninja/failure-rate";

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

/// Sets (`Some`) or removes (`None`) annotations on the CronJob.
async fn annotate(client: &Client, annotations: &[(&str, Option<&str>)]) {
    let annotations: serde_json::Map<_, _> = annotations
        .iter()
        .map(|(k, v)| ((*k).to_owned(), json!(v)))
        .collect();
    let patch = json!({ "metadata": { "annotations": annotations } });
    cron_jobs(client)
        .patch(NAME, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .unwrap();
}

/// Polls the CronJob until `done` holds, failing after `deadline`.
async fn until(
    client: &Client,
    deadline: Duration,
    what: &str,
    done: impl Fn(&CronJob) -> bool,
) -> CronJob {
    let started = Instant::now();
    loop {
        let c = get(client).await;
        if done(&c) {
            return c;
        }
        assert!(
            started.elapsed() < deadline,
            "{NS}/{NAME} never {what} (state {:?}, history {:?})",
            state(&c),
            annotation(&c, RUN_HISTORY)
        );
        tokio::time::sleep(POLL).await;
    }
}

fn is_candidate(c: &CronJob) -> bool {
    state(c).as_deref() == Some("candidate")
}

fn runs(c: &CronJob) -> usize {
    history(c).map_or(0, |h| h.runs.len())
}

/// The reaper's `/metrics` lines about this CronJob, through the API
/// server's pod proxy.
async fn series(client: &Client) -> Vec<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "k8s-reaper");
    let pod = pods
        .list(&ListParams::default().labels("app.kubernetes.io/name=k8s-reaper"))
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .max_by_key(|p| p.metadata.creation_timestamp.clone())
        .expect("the reaper is running");
    let request = http::Request::get(format!(
        "/api/v1/namespaces/k8s-reaper/pods/{}:8080/proxy/metrics",
        pod.metadata.name.unwrap()
    ))
    .body(Vec::new())
    .unwrap();
    let text = client.request_text(request).await.unwrap();
    let ours = format!("namespace=\"{NS}\",name=\"{NAME}\"");
    text.lines()
        .filter(|l| l.starts_with("reaper_cronjob_") && l.contains(&ours))
        .map(str::to_owned)
        .collect()
}

/// How many times an Event with this reason was emitted about the CronJob
/// at or after `since`, counting a series once per occurrence.
async fn emitted(client: &Client, reason: &str, since: jiff::Timestamp) -> i32 {
    let events: Api<Event> = Api::namespaced(client.clone(), NS);
    events
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .iter()
        .filter(|e| {
            let regarding = e.regarding.as_ref();
            e.reason.as_deref() == Some(reason)
                && regarding.and_then(|r| r.name.as_deref()) == Some(NAME)
                && e.event_time.as_ref().is_some_and(|t| t.0 >= since)
        })
        .map(|e| e.series.as_ref().map_or(1, |s| s.count))
        .sum()
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

/// Quickstart §2 steps 2–4: each run is recorded once, and the rate makes
/// it a candidate in report mode without suspending it.
async fn becomes_a_candidate_on_the_rate(client: &Client) {
    let since = before();
    cron_jobs(client)
        .create(&PostParams::default(), &cron_job())
        .await
        .unwrap();

    let started = Instant::now();
    let mut seen = BTreeSet::new();
    let c = loop {
        let c = get(client).await;
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
    expect_event(client, "ReapPlanned", RATE_CLAUSE, since).await;
    let rule = format!(
        "reaper_cronjob_rule{{namespace=\"{NS}\",name=\"{NAME}\",rule=\"failure_rate\"}} 1"
    );
    let lines = series(client).await;
    assert!(lines.contains(&rule), "no {rule} in {lines:?}");
}

/// Quickstart §4, while it is a candidate: an annotation only raises the
/// window.
async fn a_window_override_only_raises_it(client: &Client) {
    // A window below the cluster's is ignored, and quietly.
    let since = before();
    annotate(client, &[(FAILURE_WINDOW, Some("3"))]).await;
    tokio::time::sleep(WITHIN).await;
    let c = get(client).await;
    assert_eq!(runs(&c), WINDOW_RUNS, "failure-window=3 shrank the window");
    assert!(
        !event(client, "ConfigInvalid", FAILURE_WINDOW, since).await,
        "failure-window=3 was reported invalid"
    );

    // A larger window must fill before it is judged.
    annotate(client, &[(FAILURE_WINDOW, Some("8"))]).await;
    let c = until(client, NEXT_RUN + WITHIN, "grew past 5 runs", |c| {
        runs(c) > WINDOW_RUNS
    })
    .await;
    assert!(
        !is_candidate(&c),
        "a candidate on a window of 8 not yet full"
    );

    // Without it, the window is 5 again and the rate is met again.
    annotate(client, &[(FAILURE_WINDOW, None)]).await;
    until(client, 2 * NEXT_RUN + WITHIN, "a candidate again", |c| {
        is_candidate(c) && runs(c) == WINDOW_RUNS
    })
    .await;
}

/// Quickstart §2 step 5 (SC-006): full mode suspends it, naming the rate.
async fn full_mode_suspends_it(client: &Client) {
    let since = before();
    set_mode(client, "full").await;
    let started = Instant::now();
    loop {
        let c = get(client).await;
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
    expect_event(client, "Reaped", RATE_CLAUSE, since).await;
}

/// Quickstart §2 step 6: a restore starts the history over, and Jobs from
/// before it are not recorded again.
async fn a_restore_starts_over(client: &Client) {
    let patch = json!({ "metadata": { "annotations": { RESTORE: "now" } } });
    cron_jobs(client)
        .patch(NAME, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .unwrap();
    let c = until(client, WITHIN, "resumed", |c| !suspended(c)).await;

    for key in [RUN_HISTORY, FAILING_SINCE, FAILED_RUNS] {
        assert_eq!(annotation(&c, key), None, "{key} kept after the restore");
    }
    let c = until(client, NEXT_RUN + WITHIN, "recorded a failure", |c| {
        history(c).is_some()
    })
    .await;
    assert_eq!(runs(&c), 1, "history after the restore: {:?}", history(&c));
    assert!(!is_candidate(&c), "a candidate on one run");
}

/// Quickstart §4, after the restore: an unreadable window is reported, and
/// turning the rule off for this CronJob drops its history.
async fn bad_and_off_overrides(client: &Client) {
    let since = before();
    annotate(client, &[(FAILURE_WINDOW, Some("x"))]).await;
    expect_event(
        client,
        "ConfigInvalid",
        &format!("{FAILURE_WINDOW}=\"x\""),
        since,
    )
    .await;
    annotate(
        client,
        &[(FAILURE_WINDOW, None), (FAILURE_RATE, Some("off"))],
    )
    .await;
    until(client, WITHIN, "dropped its history", |c| {
        history(c).is_none()
    })
    .await;
}

/// Quickstart §3: once every run succeeds, the failures leave the window and
/// nothing is left on the CronJob.
async fn a_healthy_cron_job_carries_nothing(client: &Client) {
    annotate(client, &[(FAILURE_RATE, None)]).await;
    until(client, NEXT_RUN + WITHIN, "recorded a failure again", |c| {
        history(c).is_some()
    })
    .await;
    let since = before();
    let healthy = json!({ "spec": { "jobTemplate": { "spec": { "template": { "spec": {
        "containers": [{ "name": "app", "command": ["true"] }]
    }}}}}});
    cron_jobs(client)
        .patch(NAME, &PatchParams::default(), &Patch::Strategic(&healthy))
        .await
        .unwrap();
    let deadline = Duration::from_mins(WINDOW_RUNS as u64 + 2) + WITHIN;
    let c = until(client, deadline, "let go", |c| {
        history(c).is_none() && state(c).is_none()
    })
    .await;
    assert_eq!(annotation(&c, FAILING_SINCE), None);
    tokio::time::sleep(WITHIN).await;
    assert_eq!(emitted(client, "JobFailuresRecovered", since).await, 1);
    let lines = series(client).await;
    assert!(lines.is_empty(), "series left for {NS}/{NAME}: {lines:?}");
}

#[tokio::test]
#[ignore = "needs the dev cluster with the reaper running (make dev-kind dev-run)"]
async fn a_cron_job_that_fails_most_runs_is_suspended() {
    let client = common::dev_client().await;
    fresh_namespace(&client).await;
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
    becomes_a_candidate_on_the_rate(&client).await;
    a_window_override_only_raises_it(&client).await;
    full_mode_suspends_it(&client).await;
    a_restore_starts_over(&client).await;
    bad_and_off_overrides(&client).await;
    a_healthy_cron_job_carries_nothing(&client).await;
    drop(config);
    delete_cron_job(&client).await;
}
