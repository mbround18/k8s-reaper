//! Crash-loop detection: container classification (FR-001), owner
//! resolution (FR-002), and CronJob failing streaks (FR-028 to FR-030).

use std::collections::BTreeMap;

use crate::model::{
    ContainerView, CronJobView, JobOutcome, NsName, OwnerRef, PodView, Snapshot, WorkloadKind,
    WorkloadRef,
};
use crate::plan::UnsupportedPod;
use crate::record::WorkloadRecord;
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
    /// No finished Job to judge by: the record stays as it is.
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
    let mut finished: Vec<_> = cron
        .jobs
        .iter()
        .filter_map(|j| Some((j.finished_at()?, j)))
        .collect();
    finished.sort_by(|(ta, a), (tb, b)| ta.cmp(tb).then_with(|| a.name.cmp(&b.name)));
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
        None => (*first_at, 0, &failed[..]),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Termination;
    use crate::testing::{
        add_pod, crashlooping_container, deployment, failed_job, healthy_container, mins_ago,
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
}
