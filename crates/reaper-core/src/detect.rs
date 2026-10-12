//! Crash-loop detection: container classification (FR-001), owner
//! resolution (FR-002), and CronJob failing streaks (FR-028 to FR-030).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::model::{
    ContainerView, CronJobView, JobOutcome, JobView, NsName, OwnerRef, PodView, Snapshot,
    WorkloadKind, WorkloadRef,
};
use crate::plan::{RateEvidence, UnsupportedPod};
use crate::record::{RecordedRun, RunHistory, WorkloadRecord};
use crate::time::Timestamp;

const CRASH_LOOP_BACK_OFF: &str = "CrashLoopBackOff";

/// A container is crash-looping when it is not ready and is either waiting in
/// `CrashLoopBackOff` or currently terminated with an error. Image pull
/// failures and clean exits do not count.
#[must_use]
pub fn is_crash_looping(c: &ContainerView) -> bool {
    if c.ready {
        return false;
    }
    if c.waiting_reason.as_deref() == Some(CRASH_LOOP_BACK_OFF) {
        return true;
    }
    c.terminated.as_ref().is_some_and(|t| t.exit_code != 0)
}

/// Where a pod belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodOwner {
    Workload(WorkloadRef),
    /// Job pods belong to CronJob handling (US5), never to crash-loop detection.
    Job,
    /// A bare pod or an owner the reaper cannot act on; `None` for a bare pod.
    Unsupported(Option<String>),
}

/// Pod → ReplicaSet → Deployment, Pod → StatefulSet, Pod → DaemonSet.
#[must_use]
pub fn resolve_owner(pod: &PodView, replica_sets: &BTreeMap<NsName, Option<OwnerRef>>) -> PodOwner {
    let Some(owner) = &pod.owner else {
        return PodOwner::Unsupported(None);
    };
    let workload = |kind| PodOwner::Workload(WorkloadRef::new(kind, &pod.namespace, &owner.name));
    match owner.kind.as_str() {
        "StatefulSet" => workload(WorkloadKind::StatefulSet),
        "DaemonSet" => workload(WorkloadKind::DaemonSet),
        "Job" => PodOwner::Job,
        "ReplicaSet" => match replica_sets.get(&NsName::new(&pod.namespace, &owner.name)) {
            Some(Some(rs_owner)) if rs_owner.kind == "Deployment" && rs_owner.controller => {
                PodOwner::Workload(WorkloadRef::new(
                    WorkloadKind::Deployment,
                    &pod.namespace,
                    &rs_owner.name,
                ))
            }
            _ => PodOwner::Unsupported(Some(owner.kind.clone())),
        },
        other => PodOwner::Unsupported(Some(other.to_owned())),
    }
}

/// The containers of one workload's pods, flattened, with their pod name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkloadPods<'a> {
    pub containers: Vec<(&'a str, &'a ContainerView)>,
}

impl<'a> WorkloadPods<'a> {
    #[must_use]
    pub fn crash_looping(&self) -> bool {
        self.containers.iter().any(|(_, c)| is_crash_looping(c))
    }

    /// The crash-looping container with the most restarts, or, when none is
    /// crash-looping, the container with the most restarts. Ties go to pod
    /// name, then container name, so the choice is deterministic.
    #[must_use]
    pub fn worst(&self) -> Option<(&'a str, &'a ContainerView)> {
        let looping = self.crash_looping();
        self.containers
            .iter()
            .filter(|(_, c)| !looping || is_crash_looping(c))
            .min_by(|(pa, a), (pb, b)| {
                b.restart_count
                    .cmp(&a.restart_count)
                    .then_with(|| pa.cmp(pb))
                    .then_with(|| a.name.cmp(&b.name))
            })
            .copied()
    }
}

/// Pods grouped by workload, plus crash-looping pods that cannot be
/// attributed to a supported workload. `in_scope` filters namespaces.
#[must_use]
pub fn group_pods<'a>(
    snap: &'a Snapshot,
    in_scope: impl Fn(&str) -> bool,
) -> (BTreeMap<WorkloadRef, WorkloadPods<'a>>, Vec<UnsupportedPod>) {
    let mut by_workload: BTreeMap<WorkloadRef, WorkloadPods<'a>> = BTreeMap::new();
    let mut unsupported = Vec::new();
    for pod in snap.pods.iter().filter(|p| in_scope(&p.namespace)) {
        match resolve_owner(pod, &snap.replica_sets) {
            PodOwner::Workload(w) => by_workload
                .entry(w)
                .or_default()
                .containers
                .extend(pod.containers.iter().map(|c| (pod.name.as_str(), c))),
            PodOwner::Job => {}
            PodOwner::Unsupported(owner_kind) => {
                if pod.containers.iter().any(is_crash_looping) {
                    unsupported.push(UnsupportedPod {
                        namespace: pod.namespace.clone(),
                        pod: pod.name.clone(),
                        owner_kind,
                    });
                }
            }
        }
    }
    unsupported.sort();
    (by_workload, unsupported)
}

/// A CronJob's recent runs as the reaper reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStreak {
    /// No finished Job to judge by, or none failed since the last one
    /// counted before a restore: the record stays as it is.
    Unknown,
    /// The most recent finished Job succeeded.
    Passing,
    Failing(Streak),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Streak {
    /// Completion time of the first failed Job after the last success.
    pub since: Timestamp,
    /// Distinct failed Jobs in the streak, including pruned ones counted
    /// before.
    pub failed_runs: u32,
    pub last_failed_job: String,
    pub last_failure_reason: Option<String>,
}

/// Reads the streak from the Jobs still listed and what `record` counted
/// before. Running Jobs are ignored. Jobs are pruned oldest first, so once
/// the last counted Job is gone every listed failure is a new one.
#[must_use]
pub fn job_streak(cron: &CronJobView, record: &WorkloadRecord) -> JobStreak {
    let finished = finished_jobs(cron);
    let Some((_, latest)) = finished.last() else {
        return JobStreak::Unknown;
    };
    if !latest.failed() {
        return JobStreak::Passing;
    }
    let last_success = finished
        .iter()
        .filter(|(_, j)| !j.failed())
        .map(|(t, _)| *t)
        .max()
        .max(cron.last_successful_time);
    let after_success = |t: Timestamp| last_success.is_none_or(|s| t > s);
    let failed: Vec<_> = finished
        .iter()
        .filter(|(t, j)| j.failed() && after_success(*t))
        .collect();
    let (Some((first_at, _)), Some((_, last))) = (failed.first(), failed.last()) else {
        return JobStreak::Passing;
    };

    // The recorded streak holds unless a success came after it began.
    let (since, counted, new) = match record.failing_since.filter(|t| after_success(*t)) {
        Some(since) => {
            let seen = record
                .last_failed_job
                .as_deref()
                .and_then(|name| failed.iter().position(|(_, j)| j.name == name));
            let new = seen.map_or(&failed[..], |i| &failed[i + 1..]);
            (since, record.failed_runs.unwrap_or(0), new)
        }
        // After a restore only `last-failed-job` is left: the streak starts
        // with the first failure after it.
        None => match record
            .last_failed_job
            .as_deref()
            .and_then(|name| failed.iter().position(|(_, j)| j.name == name))
        {
            Some(i) if i + 1 == failed.len() => return JobStreak::Unknown,
            Some(i) => (failed[i + 1].0, 0, &failed[i + 1..]),
            None => (*first_at, 0, &failed[..]),
        },
    };
    let new = u32::try_from(new.len()).unwrap_or(u32::MAX);
    JobStreak::Failing(Streak {
        since,
        failed_runs: counted.saturating_add(new),
        last_failed_job: last.name.clone(),
        last_failure_reason: match &last.outcome {
            JobOutcome::Failed { reason, .. } => reason.clone(),
            _ => None,
        },
    })
}

/// Finished Jobs ordered by `(finish time, name)`.
fn finished_jobs(cron: &CronJobView) -> Vec<(Timestamp, &JobView)> {
    let mut finished: Vec<_> = cron
        .jobs
        .iter()
        .filter_map(|j| Some((j.finished_at()?, j)))
        .collect();
    finished.sort_by(|(ta, a), (tb, b)| ta.cmp(tb).then_with(|| a.name.cmp(&b.name)));
    finished
}

/// Records this cycle's finished runs in the history (spec 003, research
/// R2–R4). A history only begins from the current failing streak; after
/// that every finished Job past the newest recorded run is appended, plus at
/// most one success inferred from `lastSuccessfulTime`. Unseen failures are
/// never inferred. The result is trimmed to `window_runs` and is `None` once
/// it holds no failure.
#[must_use]
pub fn update_history(
    prev: Option<RunHistory>,
    cron: &CronJobView,
    streak: &JobStreak,
    window_runs: u32,
) -> Option<RunHistory> {
    let (since, mut runs, from) = match (prev, streak) {
        (Some(h), _) => (Some(h.since), h.runs, None),
        (None, JobStreak::Failing(s)) => (None, Vec::new(), Some(s.since)),
        (None, _) => return None,
    };
    // A seeded history has no watermark, and every success is before it.
    let watermark = runs.last().cloned();
    for (at, job) in finished_jobs(cron) {
        let new = runs.last().is_none_or(|l| l.is_before(at, Some(&job.name)));
        if new && from.is_none_or(|f| at >= f) {
            runs.push(RecordedRun {
                job: Some(job.name.clone()),
                at,
                ok: !job.failed(),
            });
        }
    }
    let since = since.or_else(|| runs.first().map(|r| r.at))?;
    // The deleted success may come before a failure still listed, so it
    // is placed by time, not appended.
    if let Some(lst) = cron.last_successful_time {
        let seen = runs.iter().any(|r| r.at == lst);
        if !seen && watermark.is_some_and(|w| w.is_before(lst, None)) {
            let i = runs.partition_point(|r| r.at < lst);
            runs.insert(
                i,
                RecordedRun {
                    job: None,
                    at: lst,
                    ok: true,
                },
            );
        }
    }
    let keep = usize::try_from(window_runs).unwrap_or(usize::MAX);
    runs.drain(..runs.len().saturating_sub(keep));
    runs.iter()
        .any(|r| !r.ok)
        .then_some(RunHistory { since, runs })
}

/// The failure-rate rule (FR-005): the window is full, at least
/// `threshold` percent of it failed, the newest run failed, and the history
/// began at least `failing_duration` ago. Integer arithmetic only.
#[must_use]
pub fn rate_met(
    h: &RunHistory,
    enabled: bool,
    window_runs: u32,
    threshold: u8,
    failing_duration: Duration,
    now: Timestamp,
) -> RateEvidence {
    let failed = h.failed();
    let window = u32::try_from(h.runs.len()).unwrap_or(u32::MAX);
    let met = enabled
        && window == window_runs
        && u64::from(failed) * 100 >= u64::from(threshold) * u64::from(window)
        && h.runs.last().is_some_and(|r| !r.ok)
        && now.elapsed_at_least(h.since, failing_duration);
    RateEvidence {
        failed,
        window,
        met,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Termination;
    use crate::testing::{
        NOW, add_pod, crashlooping_container, deployment, failed_job, healthy_container, mins_ago,
        running_job, snapshot, succeeded_job, workload,
    };

    fn terminated(exit_code: i32, reason: &str) -> ContainerView {
        ContainerView {
            name: "app".into(),
            restart_count: 3,
            terminated: Some(Termination {
                exit_code,
                reason: reason.into(),
            }),
            ..ContainerView::default()
        }
    }

    #[test]
    fn crash_loop_back_off_is_crash_looping() {
        assert!(is_crash_looping(&crashlooping_container("api", 12)));
    }

    #[test]
    fn terminated_with_an_error_is_crash_looping() {
        assert!(is_crash_looping(&terminated(1, "Error")));
        assert!(is_crash_looping(&terminated(137, "OOMKilled")));
        assert!(!is_crash_looping(&terminated(0, "Completed")));
    }

    #[test]
    fn init_containers_count() {
        let c = ContainerView {
            init: true,
            ..crashlooping_container("migrate", 11)
        };
        assert!(is_crash_looping(&c));
    }

    #[test]
    fn image_pull_back_off_is_not_crash_looping() {
        let c = ContainerView {
            name: "app".into(),
            waiting_reason: Some("ImagePullBackOff".into()),
            ..ContainerView::default()
        };
        assert!(!is_crash_looping(&c));
    }

    #[test]
    fn ready_containers_are_not_crash_looping() {
        assert!(!is_crash_looping(&healthy_container("app")));
        let ready_but_reported = ContainerView {
            ready: true,
            ..crashlooping_container("api", 12)
        };
        assert!(!is_crash_looping(&ready_but_reported));
    }

    fn pod(owner: Option<(&str, &str)>) -> PodView {
        PodView {
            namespace: "app".into(),
            name: "p".into(),
            owner: owner.map(|(kind, name)| OwnerRef {
                kind: kind.into(),
                name: name.into(),
                controller: true,
            }),
            containers: vec![crashlooping_container("api", 12)],
        }
    }

    #[test]
    fn deployment_resolves_through_its_replicaset() {
        let mut snap = snapshot();
        add_pod(
            &mut snap,
            &deployment("app", "web").workload,
            "web-1",
            vec![],
        );
        assert_eq!(
            resolve_owner(&snap.pods[0], &snap.replica_sets),
            PodOwner::Workload(WorkloadRef::new(WorkloadKind::Deployment, "app", "web"))
        );
    }

    #[test]
    fn statefulset_and_daemonset_pods_resolve_directly() {
        let snap = snapshot();
        for kind in [WorkloadKind::StatefulSet, WorkloadKind::DaemonSet] {
            let w = workload(kind, "app", "db").workload;
            assert_eq!(
                resolve_owner(&pod(Some((kind.as_str(), "db"))), &snap.replica_sets),
                PodOwner::Workload(w)
            );
        }
    }

    #[test]
    fn job_pods_are_ignored_and_bare_pods_unsupported() {
        let snap = snapshot();
        assert_eq!(
            resolve_owner(&pod(Some(("Job", "nightly-1"))), &snap.replica_sets),
            PodOwner::Job
        );
        assert_eq!(
            resolve_owner(&pod(None), &snap.replica_sets),
            PodOwner::Unsupported(None)
        );
        assert_eq!(
            resolve_owner(&pod(Some(("Rollout", "web"))), &snap.replica_sets),
            PodOwner::Unsupported(Some("Rollout".into()))
        );
    }

    #[test]
    fn orphan_replicaset_is_unsupported() {
        let mut snap = snapshot();
        snap.replica_sets.insert(NsName::new("app", "orphan"), None);
        assert_eq!(
            resolve_owner(&pod(Some(("ReplicaSet", "orphan"))), &snap.replica_sets),
            PodOwner::Unsupported(Some("ReplicaSet".into()))
        );
    }

    #[test]
    fn grouping_reports_only_crash_looping_unsupported_pods() {
        let mut snap = snapshot();
        let mut bare = pod(None);
        bare.name = "bare".into();
        let mut quiet = pod(None);
        quiet.name = "quiet".into();
        quiet.containers = vec![healthy_container("app")];
        let mut job = pod(Some(("Job", "nightly-1")));
        job.name = "job".into();
        let mut excluded = pod(None);
        excluded.namespace = "kube-system".into();
        snap.pods.extend([bare, quiet, job, excluded]);
        add_pod(
            &mut snap,
            &deployment("app", "web").workload,
            "web-1",
            vec![crashlooping_container("api", 12)],
        );

        let (groups, unsupported) = group_pods(&snap, |ns| ns != "kube-system");
        assert_eq!(
            unsupported,
            vec![UnsupportedPod {
                namespace: "app".into(),
                pod: "bare".into(),
                owner_kind: None,
            }]
        );
        assert_eq!(groups.len(), 1);
        assert!(groups.values().next().unwrap().crash_looping());
    }

    #[test]
    fn worst_prefers_crash_looping_then_most_restarts() {
        let busy_but_fine = ContainerView {
            restart_count: 50,
            ..healthy_container("sidecar")
        };
        let a = crashlooping_container("api", 12);
        let b = crashlooping_container("worker", 14);
        let pods = WorkloadPods {
            containers: vec![("p1", &busy_but_fine), ("p1", &a), ("p2", &b)],
        };
        assert_eq!(
            pods.worst().map(|(p, c)| (p, c.name.as_str())),
            Some(("p2", "worker"))
        );

        let healthy = WorkloadPods {
            containers: vec![("p1", &busy_but_fine)],
        };
        assert_eq!(healthy.worst().map(|(_, c)| c.restart_count), Some(50));
        assert!(!healthy.crash_looping());
    }

    // US5: CronJob streaks (T080)

    fn cron(jobs: Vec<crate::model::JobView>) -> CronJobView {
        CronJobView {
            jobs,
            ..CronJobView::default()
        }
    }

    fn failing(streak: JobStreak) -> Streak {
        match streak {
            JobStreak::Failing(s) => s,
            other => panic!("not failing: {other:?}"),
        }
    }

    #[test]
    fn the_streak_starts_at_the_first_failure_after_the_last_success() {
        let mut c = cron(vec![
            failed_job("j-1", 50),
            succeeded_job("j-2", 40),
            failed_job("j-4", 20),
            failed_job("j-3", 30),
            failed_job("j-5", 10),
        ]);
        c.last_successful_time = Some(mins_ago(40));
        let s = failing(job_streak(&c, &WorkloadRecord::default()));
        assert_eq!(s.since, mins_ago(30));
        assert_eq!(s.failed_runs, 3);
        assert_eq!(s.last_failed_job, "j-5");
        assert_eq!(
            s.last_failure_reason.as_deref(),
            Some("BackoffLimitExceeded")
        );
    }

    #[test]
    fn last_successful_time_bounds_the_streak_when_the_success_was_pruned() {
        let mut c = cron(vec![failed_job("j-1", 50), failed_job("j-3", 10)]);
        c.last_successful_time = Some(mins_ago(30));
        let s = failing(job_streak(&c, &WorkloadRecord::default()));
        assert_eq!((s.since, s.failed_runs), (mins_ago(10), 1));
    }

    #[test]
    fn running_jobs_are_ignored() {
        let c = cron(vec![failed_job("j-1", 10), running_job("j-2")]);
        let s = failing(job_streak(&c, &WorkloadRecord::default()));
        assert_eq!(s.last_failed_job, "j-1");
        assert_eq!(
            job_streak(&cron(vec![running_job("j-1")]), &WorkloadRecord::default()),
            JobStreak::Unknown
        );
        assert_eq!(
            job_streak(&cron(Vec::new()), &WorkloadRecord::default()),
            JobStreak::Unknown
        );
    }

    #[test]
    fn a_success_ends_the_streak() {
        let c = cron(vec![failed_job("j-1", 20), succeeded_job("j-2", 10)]);
        assert_eq!(
            job_streak(&c, &WorkloadRecord::default()),
            JobStreak::Passing
        );
    }

    fn recorded(since: i64, runs: u32, last: &str) -> WorkloadRecord {
        WorkloadRecord {
            failing_since: Some(mins_ago(since)),
            failed_runs: Some(runs),
            last_failed_job: Some(last.into()),
            ..WorkloadRecord::default()
        }
    }

    #[test]
    fn a_job_is_counted_once_through_last_failed_job() {
        let c = cron(vec![failed_job("j-6", 20), failed_job("j-7", 10)]);
        // j-6 was counted last cycle.
        let s = failing(job_streak(&c, &recorded(300, 6, "j-6")));
        assert_eq!((s.since, s.failed_runs), (mins_ago(300), 7));
        assert_eq!(s.last_failed_job, "j-7");
        // Nothing new.
        let s = failing(job_streak(&c, &recorded(300, 7, "j-7")));
        assert_eq!(s.failed_runs, 7);
    }

    #[test]
    fn once_the_last_counted_job_is_pruned_every_listed_failure_is_new() {
        let c = cron(vec![failed_job("j-8", 20), failed_job("j-9", 10)]);
        let s = failing(job_streak(&c, &recorded(300, 7, "j-7")));
        assert_eq!(s.failed_runs, 9);
    }

    #[test]
    fn a_success_missed_between_cycles_starts_a_new_streak() {
        let mut c = cron(vec![failed_job("j-9", 10)]);
        c.last_successful_time = Some(mins_ago(15));
        let s = failing(job_streak(&c, &recorded(300, 7, "j-7")));
        assert_eq!((s.since, s.failed_runs), (mins_ago(10), 1));
    }

    #[test]
    fn after_a_restore_only_jobs_after_the_last_counted_one_count() {
        // A restore leaves only `last-failed-job`: the streak starts again.
        let c = cron(vec![
            failed_job("j-1", 400),
            failed_job("j-2", 300),
            failed_job("j-3", 20),
        ]);
        let restored = WorkloadRecord {
            last_failed_job: Some("j-2".into()),
            ..WorkloadRecord::default()
        };
        let s = failing(job_streak(&c, &restored));
        assert_eq!((s.since, s.failed_runs), (mins_ago(20), 1));
        assert_eq!(s.last_failed_job, "j-3");

        let quiet = cron(vec![failed_job("j-1", 400), failed_job("j-2", 300)]);
        assert_eq!(job_streak(&quiet, &restored), JobStreak::Unknown);
    }

    #[test]
    fn job_pods_never_count_as_crash_loops() {
        let mut snap = snapshot();
        let c = workload(WorkloadKind::CronJob, "app", "ingest");
        snap.pods.push(PodView {
            namespace: "app".into(),
            name: "ingest-1-abc".into(),
            owner: Some(OwnerRef {
                kind: "Job".into(),
                name: "ingest-1".into(),
                controller: true,
            }),
            containers: vec![crashlooping_container("main", 40)],
        });
        snap.workloads.push(c);
        let (by_workload, unsupported) = group_pods(&snap, |_| true);
        assert!(by_workload.is_empty());
        assert_eq!(unsupported, Vec::<UnsupportedPod>::new());
    }

    // Spec 003, US2: the run history (T210, T212)

    fn rec(job: Option<&str>, minutes: i64, ok: bool) -> RecordedRun {
        RecordedRun {
            job: job.map(str::to_owned),
            at: mins_ago(minutes),
            ok,
        }
    }

    fn hist(since: i64, runs: Vec<RecordedRun>) -> RunHistory {
        RunHistory {
            since: mins_ago(since),
            runs,
        }
    }

    /// One cycle as `decide` will run it, with a fresh streak record.
    fn step(prev: Option<RunHistory>, c: &CronJobView, window: u32) -> Option<RunHistory> {
        step_with(prev, c, &WorkloadRecord::default(), window)
    }

    fn step_with(
        prev: Option<RunHistory>,
        c: &CronJobView,
        record: &WorkloadRecord,
        window: u32,
    ) -> Option<RunHistory> {
        update_history(prev, c, &job_streak(c, record), window)
    }

    fn one_failure() -> RunHistory {
        hist(30, vec![rec(Some("j-1"), 30, false)])
    }

    #[test]
    fn history_none_while_passing() {
        let c = cron(vec![succeeded_job("j-1", 10)]);
        assert_eq!(step(None, &c, 10), None);
    }

    #[test]
    fn history_seeded_from_the_failing_streak() {
        let c = cron(vec![
            failed_job("j-3", 10),
            failed_job("j-1", 30),
            failed_job("j-2", 20),
        ]);
        assert_eq!(
            step(None, &c, 10),
            Some(hist(
                30,
                vec![
                    rec(Some("j-1"), 30, false),
                    rec(Some("j-2"), 20, false),
                    rec(Some("j-3"), 10, false),
                ]
            ))
        );
    }

    #[test]
    fn history_relisting_adds_nothing() {
        let c = cron(vec![failed_job("j-1", 30), failed_job("j-2", 20)]);
        let h = step(None, &c, 10);
        assert_eq!(step(h.clone(), &c, 10), h);
        assert_eq!(h.unwrap().runs.len(), 2);
    }

    #[test]
    fn history_survives_a_restart() {
        let c = cron(vec![failed_job("j-1", 30), succeeded_job("j-2", 20)]);
        let h = step(Some(one_failure()), &c, 10).unwrap();
        let reread = RunHistory::decode(&h.encode());
        assert_eq!(reread.as_ref(), Some(&h));
        assert_eq!(step(reread, &c, 10), Some(h));
    }

    #[test]
    fn history_orders_by_finish_time() {
        // j-2 started first but finished last.
        let c = cron(vec![
            failed_job("j-1", 30),
            failed_job("j-2", 10),
            succeeded_job("j-3", 20),
        ]);
        assert_eq!(
            step(Some(one_failure()), &c, 10),
            Some(hist(
                30,
                vec![
                    rec(Some("j-1"), 30, false),
                    rec(Some("j-3"), 20, true),
                    rec(Some("j-2"), 10, false),
                ]
            ))
        );
    }

    #[test]
    fn history_infers_a_deleted_success() {
        let mut c = cron(vec![failed_job("j-1", 30)]);
        c.last_successful_time = Some(mins_ago(20));
        assert_eq!(
            step(Some(one_failure()), &c, 10),
            Some(hist(
                30,
                vec![rec(Some("j-1"), 30, false), rec(None, 20, true)]
            ))
        );
    }

    #[test]
    fn history_infers_a_deleted_success_before_a_listed_failure() {
        // j-2 succeeded at 20 and was deleted; j-3 failed after it.
        let mut c = cron(vec![failed_job("j-1", 30), failed_job("j-3", 10)]);
        c.last_successful_time = Some(mins_ago(20));
        assert_eq!(
            step(Some(one_failure()), &c, 10).unwrap().runs,
            [
                rec(Some("j-1"), 30, false),
                rec(None, 20, true),
                rec(Some("j-3"), 10, false),
            ]
        );
    }

    #[test]
    fn history_does_not_infer_a_listed_success() {
        let mut c = cron(vec![failed_job("j-1", 30), succeeded_job("j-2", 20)]);
        c.last_successful_time = Some(mins_ago(20));
        let h = step(Some(one_failure()), &c, 10).unwrap();
        assert_eq!(
            h.runs,
            [rec(Some("j-1"), 30, false), rec(Some("j-2"), 20, true)]
        );
        // And not later either, once the Job is gone.
        let gone = cron(vec![failed_job("j-1", 30)]);
        let gone = CronJobView {
            last_successful_time: Some(mins_ago(20)),
            ..gone
        };
        assert_eq!(step(Some(h.clone()), &gone, 10), Some(h));
    }

    #[test]
    fn history_infers_at_most_one_success_per_cycle() {
        // Successes at 25, 15 and 5 were all deleted before this cycle.
        let mut c = cron(vec![failed_job("j-1", 30)]);
        c.last_successful_time = Some(mins_ago(5));
        let h = step(Some(one_failure()), &c, 10).unwrap();
        assert_eq!(h.runs, [rec(Some("j-1"), 30, false), rec(None, 5, true)]);
    }

    #[test]
    fn history_never_infers_a_failure() {
        // j-2 failed and was deleted before the reaper saw it.
        let c = cron(Vec::new());
        assert_eq!(step(Some(one_failure()), &c, 10), Some(one_failure()));
    }

    #[test]
    fn history_waits_for_running_jobs() {
        let c = cron(vec![failed_job("j-1", 30), running_job("j-2")]);
        assert_eq!(step(Some(one_failure()), &c, 10), Some(one_failure()));
        let c = cron(vec![failed_job("j-1", 30), failed_job("j-2", 5)]);
        assert_eq!(
            step(Some(one_failure()), &c, 10).unwrap().runs,
            [rec(Some("j-1"), 30, false), rec(Some("j-2"), 5, false)]
        );
    }

    #[test]
    fn history_skips_jobs_before_the_watermark() {
        let prev = Some(hist(
            30,
            vec![rec(Some("j-1"), 30, false), rec(Some("j-3"), 10, false)],
        ));
        let c = cron(vec![failed_job("j-2", 20), failed_job("j-3", 10)]);
        assert_eq!(step(prev.clone(), &c, 10), prev);
    }

    #[test]
    fn history_is_trimmed_to_the_window() {
        let prev = Some(hist(
            30,
            vec![
                rec(Some("j-1"), 30, false),
                rec(Some("j-2"), 20, true),
                rec(Some("j-3"), 10, false),
            ],
        ));
        let c = cron(vec![failed_job("j-4", 5)]);
        let h = step(prev.clone(), &c, 3).unwrap();
        assert_eq!(h.since, mins_ago(30));
        assert_eq!(
            h.runs.iter().map(|r| r.job.as_deref()).collect::<Vec<_>>(),
            [Some("j-2"), Some("j-3"), Some("j-4")]
        );
        // A smaller window drops the oldest runs.
        let h = step(prev, &cron(Vec::new()), 2).unwrap();
        assert_eq!(h.runs.len(), 2);
        assert_eq!(h.runs[0].job.as_deref(), Some("j-2"));
    }

    #[test]
    fn history_clears_when_the_window_holds_no_failure() {
        let c = cron(vec![
            failed_job("j-1", 30),
            succeeded_job("j-2", 20),
            succeeded_job("j-3", 10),
        ]);
        assert_eq!(step(Some(one_failure()), &c, 2), None);
        assert!(step(Some(one_failure()), &c, 3).is_some());
    }

    #[test]
    fn history_not_restarted_by_a_lingering_failed_job() {
        // failedJobsHistoryLimit keeps j-1 listed after the window cleared.
        let mut c = cron(vec![failed_job("j-1", 30), succeeded_job("j-3", 10)]);
        c.last_successful_time = Some(mins_ago(10));
        assert_eq!(step(None, &c, 3), None);
        c.jobs.remove(1);
        assert_eq!(step(None, &c, 3), None);
    }

    #[test]
    fn history_not_seeded_before_the_restore_watermark() {
        let restored = WorkloadRecord {
            last_failed_job: Some("j-2".into()),
            ..WorkloadRecord::default()
        };
        let c = cron(vec![failed_job("j-1", 400), failed_job("j-2", 300)]);
        assert_eq!(step_with(None, &c, &restored, 10), None);
        let c = cron(vec![
            failed_job("j-1", 400),
            failed_job("j-2", 300),
            failed_job("j-3", 20),
        ]);
        assert_eq!(
            step_with(None, &c, &restored, 10),
            Some(hist(20, vec![rec(Some("j-3"), 20, false)]))
        );
    }

    /// SC-003: fifty fixed cycle sequences mixing every case above. Job `i`
    /// finishes at `2i + (k + i) % 3`, so finish order differs from start
    /// order and finish times tie; outcomes, TTL deletion, history limits
    /// and the window all vary with `k`. No randomness.
    #[test]
    fn history_invariants_hold_over_fixed_sequences() {
        let (mut checked, mut inferred, mut trimmed) = (0, 0, 0);
        for k in 0..50_i64 {
            let window = 3 + u32::try_from(k % 5).unwrap();
            let limit = usize::try_from(1 + k % 3).unwrap();
            let mut all: Vec<JobView> = Vec::new();
            let mut deleted = std::collections::BTreeSet::new();
            let mut lst: Option<Timestamp> = None;
            let mut h: Option<RunHistory> = None;
            for cycle in 0..40_i64 {
                let now = 2 * cycle;
                let i = cycle;
                let at = 2 * i + (k + i) % 3;
                let name = format!("j-{i:03}");
                let ok = (k + i * i + i / 7) % 5 == 0 || (k % 4 == 0 && i % 3 == 0);
                all.push(if ok {
                    succeeded_job(&name, 1000 - at)
                } else {
                    failed_job(&name, 1000 - at)
                });
                if (k + i) % 4 == 0 {
                    deleted.insert(name);
                }
                let visible =
                    |j: &&JobView| j.finished_at().is_none_or(|t| t <= mins_ago(1000 - now));
                for j in all.iter().filter(visible) {
                    if !j.failed() {
                        lst = lst.max(j.finished_at());
                    }
                }
                let listed: Vec<JobView> = {
                    let mut f: Vec<_> = all
                        .iter()
                        .filter(visible)
                        .filter(|j| !deleted.contains(&j.name))
                        .cloned()
                        .collect();
                    f.sort_by_key(|j| (j.finished_at(), j.name.clone()));
                    let fails: Vec<_> = f.iter().filter(|j| j.failed()).cloned().collect();
                    let oks: Vec<_> = f.iter().filter(|j| !j.failed()).cloned().collect();
                    let mut keep: Vec<_> = fails[fails.len().saturating_sub(limit)..]
                        .iter()
                        .chain(&oks[oks.len().saturating_sub(limit)..])
                        .cloned()
                        .collect();
                    if (k + cycle) % 6 == 0 {
                        keep.push(running_job(&format!("j-{:03}", i + 1)));
                    }
                    keep.reverse();
                    keep
                };
                let c = CronJobView {
                    suspended: false,
                    last_successful_time: lst,
                    jobs: listed,
                };
                h = step(h, &c, window);
                let Some(hh) = &h else { continue };
                checked += 1;
                inferred += usize::from(hh.runs.iter().any(|r| r.job.is_none()));
                trimmed += usize::from(hh.runs.len() == window as usize);
                let names: Vec<_> = hh.runs.iter().filter_map(|r| r.job.clone()).collect();
                let unique: std::collections::BTreeSet<_> = names.iter().collect();
                assert_eq!(unique.len(), names.len(), "k={k} cycle={cycle} {hh:?}");
                assert!(
                    hh.runs
                        .windows(2)
                        .all(|w| w[0].is_before(w[1].at, w[1].job.as_deref())),
                    "k={k} cycle={cycle} {hh:?}"
                );
                assert!(hh.runs.len() <= window as usize);
                assert!(hh.runs.iter().any(|r| !r.ok));
                assert_eq!(RunHistory::decode(&hh.encode()).as_ref(), Some(hh));
            }
        }
        // The table must keep exercising histories, inference and trimming.
        assert!(checked > 1000 && inferred > 100 && trimmed > 1000);
    }

    // Spec 003, US1: the failure-rate rule (T220)

    /// Runs one minute apart ending a minute ago, oldest first: `F` failed,
    /// `S` succeeded. The history began `since` minutes ago.
    fn window(pattern: &str, since: i64) -> RunHistory {
        let n = i64::try_from(pattern.len()).unwrap();
        let runs = pattern
            .chars()
            .zip(0..)
            .map(|(c, i)| rec(Some(&format!("j-{i}")), n - i, c == 'S'))
            .collect();
        hist(since, runs)
    }

    fn rate(h: &RunHistory, threshold: u8) -> RateEvidence {
        rate_met(h, true, 10, threshold, Duration::from_hours(6), NOW)
    }

    fn evidence(failed: u32, window: u32, met: bool) -> RateEvidence {
        RateEvidence {
            failed,
            window,
            met,
        }
    }

    #[test]
    fn rate_met_with_a_full_window_over_the_threshold() {
        let h = window("FFSFFFFSFF", 7 * 60);
        assert_eq!(rate(&h, 80), evidence(8, 10, true));
        assert_eq!(
            rate_met(&h, false, 10, 80, Duration::from_hours(6), NOW),
            evidence(8, 10, false)
        );
    }

    #[test]
    fn rate_not_met_on_a_window_not_yet_full() {
        assert_eq!(rate(&window("FFFFFF", 7 * 60), 80), evidence(6, 6, false));
    }

    #[test]
    fn rate_not_met_when_the_newest_run_succeeded() {
        assert_eq!(
            rate(&window("FFFFFFFFFS", 7 * 60), 80),
            evidence(9, 10, false)
        );
    }

    #[test]
    fn rate_not_met_below_the_threshold() {
        assert_eq!(
            rate(&window("FFSFSFFSFF", 7 * 60), 80),
            evidence(7, 10, false)
        );
    }

    #[test]
    fn rate_not_met_before_the_failing_duration() {
        let h = window("FFSFFFFSFF", 5 * 60);
        assert_eq!(rate(&h, 80), evidence(8, 10, false));
        // Exactly the duration is enough.
        let h = window("FFSFFFFSFF", 6 * 60);
        assert_eq!(rate(&h, 80), evidence(8, 10, true));
    }

    #[test]
    fn rate_at_a_threshold_of_100_needs_every_run_failed() {
        assert_eq!(
            rate(&window("FFFFFFFFFF", 7 * 60), 100),
            evidence(10, 10, true)
        );
        assert_eq!(
            rate(&window("SFFFFFFFFF", 7 * 60), 100),
            evidence(9, 10, false)
        );
    }
}
