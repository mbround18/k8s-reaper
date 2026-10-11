//! SC-004: report mode is an honest dry run. For every US1 and US5 fixture, report
//! and full mode publish the same plan annotation, and report mode writes
//! nothing but `reaper.boop.ninja/*` metadata (FR-012).

use std::collections::BTreeMap;

use reaper_core::config::{Config, Mode};
use reaper_core::decide::decide;
use reaper_core::executor::execute;
use reaper_core::model::{NsName, PodView, Snapshot, WorkloadKind, WorkloadView};
use reaper_core::record::{
    CRASHLOOP_SINCE, FAILED_RUNS, FAILING_SINCE, HEALTHY_SINCE, LAST_FAILED_JOB, PLAN,
};
use reaper_core::testing::{
    Call, FakeEvents, FakeTelemetry, FakeWriter, add_namespace, add_pod, block_on,
    crashlooping_container, cron_job, cron_mut, deployment, failed_job, healthy_container,
    helm_release, mins_ago, snapshot, succeeded_job, workload,
};

fn observed(mut w: WorkloadView, minutes: i64) -> WorkloadView {
    w.reaper_annotations
        .insert(CRASHLOOP_SINCE.into(), mins_ago(minutes).to_string());
    w.state_label = Some("observed".into());
    w
}

fn with_pod(snap: &mut Snapshot, w: &WorkloadView, restarts: u32) {
    add_pod(
        snap,
        &w.workload,
        &format!("{}-1", w.workload.name),
        vec![crashlooping_container("api", restarts)],
    );
}

/// A CronJob whose last `runs.len()` Jobs failed, the given minutes ago.
fn failing_cron(name: &str, runs: &[i64]) -> WorkloadView {
    let mut w = cron_job("cron", name);
    cron_mut(&mut w).jobs = runs
        .iter()
        .enumerate()
        .map(|(i, m)| failed_job(&format!("{name}-{i}"), *m))
        .collect();
    w
}

/// Already observed failing since the first of `runs`, every run counted.
fn observed_cron(name: &str, runs: &[i64]) -> WorkloadView {
    let mut w = failing_cron(name, runs);
    let a = &mut w.reaper_annotations;
    a.insert(FAILING_SINCE.into(), mins_ago(runs[0]).to_string());
    a.insert(FAILED_RUNS.into(), runs.len().to_string());
    a.insert(LAST_FAILED_JOB.into(), format!("{name}-{}", runs.len() - 1));
    w.state_label = Some("observed".into());
    w
}

/// One snapshot holding every US1 and US5 situation side by side.
fn fixture() -> Snapshot {
    let mut snap = snapshot();
    let mut workloads = Vec::new();
    let mut add = |snap: &mut Snapshot, w: WorkloadView, restarts: Option<u32>| {
        if let Some(n) = restarts {
            with_pod(snap, &w, n);
        }
        workloads.push(w);
    };

    add(&mut snap, deployment("app", "fresh"), Some(3));
    add(
        &mut snap,
        observed(deployment("app", "young"), 10),
        Some(30),
    );
    add(&mut snap, observed(deployment("app", "few"), 90), Some(4));
    add(
        &mut snap,
        observed(deployment("app", "plain"), 45),
        Some(12),
    );

    let mut flux = observed(deployment("app", "flux"), 60);
    flux.flux.kustomization = Some(NsName::new("flux-system", "apps"));
    flux.flux.helm_release = Some(NsName::new("helm", "flux"));
    snap.helm_releases
        .insert(NsName::new("helm", "flux"), helm_release("helm", "flux"));
    add(&mut snap, flux, Some(15));

    let mut partial = observed(deployment("app", "partial"), 60);
    partial.available_replicas = 1;
    add(&mut snap, partial, Some(15));

    let mut owned = observed(deployment("app", "owned"), 60);
    owned.has_controller_owner = true;
    add(&mut snap, owned, Some(15));

    let keda = observed(deployment("app", "scaled"), 60);
    snap.keda_targets.insert(keda.workload.clone());
    add(&mut snap, keda, Some(15));

    let ds = observed(workload(WorkloadKind::DaemonSet, "app", "agent"), 60);
    snap.pods.push(PodView {
        namespace: "app".into(),
        name: "agent-x".into(),
        owner: Some(reaper_core::model::OwnerRef {
            kind: "DaemonSet".into(),
            name: "agent".into(),
            controller: true,
        }),
        containers: vec![crashlooping_container("agent", 40)],
    });
    add(&mut snap, ds, None);

    add_namespace(&mut snap, "quiet", Some("report"));
    add(
        &mut snap,
        observed(deployment("quiet", "web"), 60),
        Some(15),
    );

    let mut recovering = observed(deployment("app", "recovering"), 60);
    recovering
        .reaper_annotations
        .insert(HEALTHY_SINCE.into(), mins_ago(3).to_string());
    add_pod(
        &mut snap,
        &recovering.workload,
        "recovering-1",
        vec![healthy_container("api")],
    );
    add(&mut snap, recovering, None);

    let mut recovered = observed(deployment("app", "recovered"), 60);
    recovered
        .reaper_annotations
        .insert(HEALTHY_SINCE.into(), mins_ago(30).to_string());
    add(&mut snap, recovered, None);

    add(
        &mut snap,
        observed(deployment("kube-system", "dns"), 60),
        Some(50),
    );
    snap.pods.push(PodView {
        namespace: "app".into(),
        name: "bare".into(),
        owner: None,
        containers: vec![crashlooping_container("api", 50)],
    });

    add(&mut snap, failing_cron("fresh", &[30, 20]), None);
    add(&mut snap, observed_cron("young", &[60, 50, 40]), None);
    add(&mut snap, observed_cron("nightly", &[500, 400, 300]), None);
    let mut mended = observed_cron("mended", &[500, 400, 300]);
    cron_mut(&mut mended)
        .jobs
        .push(succeeded_job("mended-3", 5));
    add(&mut snap, mended, None);

    snap.workloads = workloads;
    snap
}

/// The plan annotation each write left, per target.
fn published(writer: &FakeWriter) -> BTreeMap<String, Option<String>> {
    writer
        .calls()
        .iter()
        .filter_map(|c| match c {
            Call::PatchMetadata { target, patch, .. } => patch
                .annotations
                .get(PLAN)
                .map(|v| (format!("{target:?}"), v.clone())),
            _ => None,
        })
        .collect()
}

fn run(snap: &Snapshot, mode: Mode) -> FakeWriter {
    let plan = decide(snap, &Config::default());
    let writer = FakeWriter::new();
    block_on(execute(
        snap,
        &plan,
        mode,
        &writer,
        &FakeEvents::default(),
        &FakeTelemetry::default(),
    ));
    writer
}

#[test]
fn report_and_full_publish_identical_plans() {
    let snap = fixture();
    let report = run(&snap, Mode::Report);
    let full = run(&snap, Mode::Full);
    let plans = published(&report);
    // The fixture must actually exercise reap and skip plans.
    assert!(plans.len() >= 7, "{plans:#?}");
    assert_eq!(plans, published(&full));
    let cron: Vec<_> = plans.keys().filter(|k| k.contains("CronJob")).collect();
    assert_eq!(cron.len(), 1, "{plans:#?}");
    assert!(cron[0].contains("nightly"), "{cron:?}");
    // Only the CronJob past both thresholds is suspended.
    let suspended: Vec<_> = full
        .calls()
        .into_iter()
        .filter(|c| matches!(c, Call::SetCronJobSuspend { suspend: true, .. }))
        .collect();
    assert_eq!(suspended.len(), 1, "{suspended:#?}");
    assert!(
        format!("{suspended:?}").contains("nightly"),
        "{suspended:?}"
    );
}

#[test]
fn report_mode_writes_only_reaper_metadata() {
    let calls = run(&fixture(), Mode::Report).calls();
    assert_ne!(calls, Vec::<Call>::new());
    for call in &calls {
        assert!(call.only_reaper_metadata(), "{call:?}");
    }
}
