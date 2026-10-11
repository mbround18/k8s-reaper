//! One cycle on a synthetic cluster the size of roanoke (T074, plan.md
//! Performance Goals) and a little over: 30 namespaces, 360 pods, 120 workloads,
//! some crash-looping, some Flux-managed, ten CronJobs with Job history.
//!
//! ```sh
//! cargo bench -p reaper-core --bench decide
//! ```
//!
//! Times `decide` plus `execute` against the in-memory fakes, so it measures
//! the core alone; the adapter's API calls are not in it. Exits non-zero when
//! the slowest cycle misses the 1 s goal.

use std::time::{Duration, Instant};

use reaper_core::config::{Config, Mode};
use reaper_core::decide::decide;
use reaper_core::executor::execute;
use reaper_core::model::{NsName, Snapshot, WorkloadKind, WorkloadView};
use reaper_core::record::{CRASHLOOP_SINCE, FAILED_RUNS, FAILING_SINCE, LAST_FAILED_JOB};
use reaper_core::testing::{
    FakeEvents, FakeTelemetry, FakeWriter, add_namespace, add_pod, block_on,
    crashlooping_container, cron_job, cron_mut, failed_job, healthy_container, helm_release,
    mins_ago, snapshot, succeeded_job, workload,
};

const NAMESPACES: usize = 30;
const WORKLOADS_PER_NAMESPACE: usize = 4;
const PODS_PER_WORKLOAD: usize = 3;
const CRON_JOBS: usize = 10;
const JOBS_PER_CRON_JOB: usize = 5;
const ITERATIONS: usize = 200;
const GOAL: Duration = Duration::from_secs(1);

fn kind(i: usize) -> WorkloadKind {
    match i % 10 {
        0 => WorkloadKind::StatefulSet,
        1 => WorkloadKind::DaemonSet,
        _ => WorkloadKind::Deployment,
    }
}

fn minutes(n: usize) -> i64 {
    i64::try_from(n).expect("small fixture count")
}

fn add_workload(snap: &mut Snapshot, ns: &str, i: usize) -> WorkloadView {
    let mut w = workload(kind(i), ns, &format!("app-{i}"));
    match i % 4 {
        0 => {
            w.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        }
        1 => {
            let hr = NsName::new(ns, format!("release-{i}"));
            snap.helm_releases
                .insert(hr.clone(), helm_release(ns, &hr.name));
            w.flux.helm_release = Some(hr);
        }
        _ => {}
    }
    // One in seven crash-loops; most of those have done so for long enough.
    let crashing = i.is_multiple_of(7);
    if crashing {
        w.reaper_annotations.insert(
            CRASHLOOP_SINCE.into(),
            mins_ago(5 + minutes(i % 60)).to_string(),
        );
        w.state_label = Some("observed".into());
    } else {
        w.available_replicas = 2;
    }
    for p in 0..PODS_PER_WORKLOAD {
        let container = if crashing {
            crashlooping_container("app", 20)
        } else {
            healthy_container("app")
        };
        add_pod(
            snap,
            &w.workload,
            &format!("app-{i}-{p}"),
            vec![container, healthy_container("sidecar")],
        );
    }
    w
}

fn add_cron_job(i: usize) -> WorkloadView {
    let mut w = cron_job("batch", &format!("cron-{i}"));
    let name = w.workload.name.clone();
    let failing = i.is_multiple_of(3);
    cron_mut(&mut w).jobs = (0..JOBS_PER_CRON_JOB)
        .map(|j| {
            let job = format!("{name}-{j}");
            let ago = 60 * minutes(JOBS_PER_CRON_JOB - j);
            if failing {
                failed_job(&job, ago)
            } else {
                succeeded_job(&job, ago)
            }
        })
        .collect();
    if failing {
        let a = &mut w.reaper_annotations;
        a.insert(FAILING_SINCE.into(), mins_ago(300).to_string());
        a.insert(FAILED_RUNS.into(), JOBS_PER_CRON_JOB.to_string());
        a.insert(
            LAST_FAILED_JOB.into(),
            format!("{name}-{}", JOBS_PER_CRON_JOB - 1),
        );
        w.state_label = Some("observed".into());
    }
    w
}

fn roanoke() -> Snapshot {
    let mut snap = snapshot();
    let mut workloads = Vec::new();
    for n in 0..NAMESPACES {
        let ns = format!("ns-{n}");
        add_namespace(&mut snap, &ns, None);
        for k in 0..WORKLOADS_PER_NAMESPACE {
            let w = add_workload(&mut snap, &ns, n * WORKLOADS_PER_NAMESPACE + k);
            workloads.push(w);
        }
    }
    add_namespace(&mut snap, "batch", None);
    workloads.extend((0..CRON_JOBS).map(add_cron_job));
    snap.workloads = workloads;
    snap
}

fn cycle(snap: &Snapshot, config: &Config) -> usize {
    let plan = decide(snap, config);
    let writer = FakeWriter::default();
    block_on(execute(
        snap,
        &plan,
        config.mode,
        &writer,
        &FakeEvents::default(),
        &FakeTelemetry::default(),
    ));
    plan.decisions.len()
}

/// Peak resident memory of this process, from `/proc` on Linux.
fn peak_rss() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    Some(line.trim_start_matches("VmHWM:").trim().to_owned())
}

fn main() {
    let snap = roanoke();
    let config = Config {
        mode: Mode::Full,
        ..Config::default()
    };
    let decisions = cycle(&snap, &config);
    for _ in 0..10 {
        cycle(&snap, &config);
    }

    let mut times: Vec<Duration> = (0..ITERATIONS)
        .map(|_| {
            let started = Instant::now();
            std::hint::black_box(cycle(std::hint::black_box(&snap), &config));
            started.elapsed()
        })
        .collect();
    times.sort_unstable();
    let median = times[ITERATIONS / 2];
    let p99 = times[ITERATIONS * 99 / 100];
    let max = times[ITERATIONS - 1];

    println!(
        "snapshot: {} namespaces, {} workloads, {} pods, {} HelmReleases; {decisions} decisions",
        snap.namespaces.len(),
        snap.workloads.len(),
        snap.pods.len(),
        snap.helm_releases.len(),
    );
    println!("cycle over {ITERATIONS} runs: median {median:?}, p99 {p99:?}, max {max:?}");
    if let Some(rss) = peak_rss() {
        println!("peak RSS of the bench process: {rss}");
    }
    if max >= GOAL {
        eprintln!("slowest cycle {max:?} misses the {GOAL:?} goal");
        std::process::exit(1);
    }
}
