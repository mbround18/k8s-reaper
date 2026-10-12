//! `decide()`: snapshot + config → one plan per workload (FR-003 to FR-011,
//! FR-017, FR-027, FR-034).
//!
//! The global mode is deliberately not an input: report and full mode must
//! publish identical plans (FR-006, SC-004). Only a `mode=report` label or
//! annotation turns a plan into a `ReportOnlyScope` skip.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::config::{
    CRASH_LOOP_DURATION_RANGE, Config, FAILING_DURATION_RANGE, MIN_FAILED_RUNS_RANGE,
    MIN_RESTARTS_RANGE, Mode, THRESHOLD_RANGE, WINDOW_RUNS_RANGE, parse_mode_override,
    parse_percent,
};
use crate::detect::{
    JobStreak, Streak, WorkloadPods, group_pods, job_streak, rate_met, update_history,
};
use crate::glob::any_matches;
use crate::model::{
    CronJobView, JobOutcome, NsName, ObjectKind, ObjectRef, Snapshot, WorkloadKind, WorkloadRef,
    WorkloadView,
};
use crate::plan::{
    Action, CyclePlan, Decision, Evidence, InvalidSetting, Plan, RateEvidence, RestoreKind,
    SkipReason, StaleHold,
};
use crate::record::{
    CRASHLOOP_DURATION, FAILING_DURATION, FAILURE_RATE, FAILURE_THRESHOLD, FAILURE_WINDOW,
    FLUX_RECONCILE_DISABLED, HelmReleaseHold, MIN_FAILED_RUNS, MIN_RESTARTS, MODE_KEY, RESTORE,
    RunHistory, State, WorkloadRecord,
};
use crate::time::parse_duration;

/// A change to this workload should start a cycle now rather than at the
/// next tick (T054, T088): a person asked for a restore, scaled a reaped
/// workload back up, or resumed a reaped CronJob.
#[must_use]
pub fn wants_immediate_cycle(w: &WorkloadView) -> bool {
    let restore = w
        .reaper_annotations
        .get(RESTORE)
        .is_some_and(|v| !v.is_empty());
    let reaped = w.state_label.as_deref().and_then(|s| s.parse().ok()) == Some(State::Reaped);
    restore || (reaped && brought_back(w))
}

/// A person undid the reap: scaled the workload up, or unsuspended the
/// CronJob.
fn brought_back(w: &WorkloadView) -> bool {
    match &w.cron_job {
        Some(cron) => !cron.suspended,
        None => w.spec_replicas.is_some_and(|n| n > 0),
    }
}

/// Decides every in-scope workload, and restores what the reaper still holds
/// in a namespace that left its scope (002 FR-010). Decisions come back
/// longest crash-looping first, then by namespace and name (FR-017).
#[must_use]
pub fn decide(snap: &Snapshot, config: &Config) -> CyclePlan {
    let in_scope = |ns: &str| namespace_in_scope(snap, config, ns);
    let (pods, unsupported) = group_pods(snap, in_scope);
    let mut invalid = BTreeSet::new();

    let mut decisions: Vec<Decision> = snap
        .workloads
        .iter()
        .filter_map(|w| {
            let ns = &w.workload.namespace;
            if *ns == snap.reaper_namespace {
                // Nothing there can hold a record, whatever it says.
                return None;
            }
            if !in_scope(ns) {
                return out_of_scope(snap, w);
            }
            let mode = label_mode(snap, w, &mut invalid);
            let thresholds = Thresholds::for_workload(config, w, &mut invalid);
            Some(match &w.cron_job {
                // CronJobs are decided from Jobs, not pods (US5).
                Some(cron) => decide_cron_job(snap, config, w, cron, mode, &thresholds),
                None => decide_workload(snap, config, w, pods.get(&w.workload), mode, &thresholds),
            })
        })
        .filter(|d| d.plan != Plan::Nothing)
        .collect();

    decisions.sort_by(|a, b| {
        // `None` (no ongoing crash loop) sorts after every timestamp.
        let since = |d: &Decision| d.since.map_or(i64::MAX, |t| t.0);
        since(a)
            .cmp(&since(b))
            .then_with(|| a.workload.namespace.cmp(&b.workload.namespace))
            .then_with(|| a.workload.name.cmp(&b.workload.name))
            .then_with(|| a.workload.kind.cmp(&b.workload.kind))
    });
    apply_limits(snap, config, &mut decisions);

    CyclePlan {
        decisions,
        unsupported,
        invalid_settings: invalid.into_iter().collect(),
        stale_holds: stale_holds(snap),
    }
}

/// The reaper's state label on every workload in the snapshot.
fn states(snap: &Snapshot) -> BTreeMap<&WorkloadRef, State> {
    snap.workloads
        .iter()
        .filter_map(|w| Some((&w.workload, w.state_label.as_deref()?.parse().ok()?)))
        .collect()
}

/// FR-017, in decision order: at most `maxReapsPerCycle` new reaps a cycle,
/// and none while `maxReaped` workloads are already held. A reap that is
/// already under way (`reaping`) is finished regardless.
fn apply_limits(snap: &Snapshot, config: &Config, decisions: &mut [Decision]) {
    let states = states(snap);
    let mut held = states
        .values()
        .filter(|s| matches!(s, State::Reaping | State::Reaped))
        .count();
    let mut this_cycle = 0;
    for d in decisions {
        let Plan::Reap { actions } = &mut d.plan else {
            continue;
        };
        if states.get(&d.workload) == Some(&State::Reaping) {
            continue;
        }
        let reason = if this_cycle >= config.limits.max_reaps_per_cycle as usize {
            SkipReason::RateLimited
        } else if held >= config.limits.max_reaped as usize {
            SkipReason::ReapCeilingReached
        } else {
            this_cycle += 1;
            held += 1;
            continue;
        };
        d.plan = Plan::Skip {
            reason,
            would: std::mem::take(actions),
        };
    }
}

/// HelmRelease holds naming a workload that is gone, or that the reaper no
/// longer holds (deleted and recreated, or its state was removed by hand).
fn stale_holds(snap: &Snapshot) -> Vec<StaleHold> {
    let states = states(snap);
    snap.helm_releases
        .values()
        .flat_map(|hr| {
            let (hold, _) = HelmReleaseHold::decode(&hr.reaper_annotations);
            let states = &states;
            hold.held_for.into_iter().filter_map(move |workload| {
                let still_held =
                    matches!(states.get(&workload), Some(State::Reaping | State::Reaped));
                (!still_held).then(|| StaleHold {
                    helm_release: hr.helm_release.clone(),
                    workload,
                })
            })
        })
        .collect()
}

/// FR-011: never the reaper's own namespace, never an excluded one, and only
/// included ones when an include list is set. A workload outside scope is
/// not observed or reaped; one the reaper is reaping or has reaped there is
/// restored (`out_of_scope`, 002 FR-010).
fn namespace_in_scope(snap: &Snapshot, config: &Config, ns: &str) -> bool {
    ns != snap.reaper_namespace
        && !any_matches(&config.scope.exclude_namespaces, ns)
        && (config.scope.include_namespaces.is_empty()
            || any_matches(&config.scope.include_namespaces, ns))
}

/// A workload outside scope (002 FR-010, data-model rule #1): restore it as
/// `opted-out` when the reaper is reaping or has reaped it; otherwise it is
/// not decided at all, so no Event is emitted and no label is written.
/// Mode labels and threshold overrides are not read: nothing else applies.
fn out_of_scope(snap: &Snapshot, w: &WorkloadView) -> Option<Decision> {
    let (record, _) = WorkloadRecord::decode(&w.reaper_annotations, w.state_label.as_deref());
    if !matches!(record.state, Some(State::Reaping | State::Reaped)) {
        return None;
    }
    Some(Decision {
        workload: w.workload.clone(),
        plan: held_plan(snap, w, &record, Mode::Off),
        since: record.crashloop_since.or(record.failing_since),
        healthy_since: None,
        evidence: None,
        run_history: record.run_history,
    })
}

/// The most restrictive mode set by labels or annotations on the namespace and
/// the workload, `Full` when none is set (FR-010). On one object the label
/// wins over the annotation. Invalid values are reported and ignored.
fn label_mode(snap: &Snapshot, w: &WorkloadView, invalid: &mut BTreeSet<InvalidSetting>) -> Mode {
    let mut read = |object: ObjectRef, label: Option<&String>, annotation: Option<&String>| {
        let Some(value) = label.or(annotation) else {
            return Mode::Full;
        };
        parse_mode_override(value).unwrap_or_else(|| {
            invalid.insert(InvalidSetting {
                object,
                key: MODE_KEY.into(),
                value: value.clone(),
            });
            Mode::Full
        })
    };
    let ns_mode = snap
        .namespaces
        .get(&w.workload.namespace)
        .map_or(Mode::Full, |ns| {
            read(
                ObjectRef {
                    kind: ObjectKind::Namespace,
                    namespace: None,
                    name: ns.name.clone(),
                },
                ns.mode_label.as_ref(),
                ns.mode_annotation.as_ref(),
            )
        });
    let workload_mode = read(
        w.workload.object_ref(),
        w.mode_label.as_ref(),
        w.mode_annotation.as_ref(),
    );
    ns_mode.min(workload_mode)
}

/// Effective thresholds: an override can only raise a global value (FR-034).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Thresholds {
    crash_loop_duration: Duration,
    min_restarts: u32,
    failing_duration: Duration,
    min_failed_runs: u32,
    failure_rate_enabled: bool,
    window_runs: u32,
    threshold: u8,
}

impl Thresholds {
    fn for_workload(
        config: &Config,
        w: &WorkloadView,
        invalid: &mut BTreeSet<InvalidSetting>,
    ) -> Self {
        let mut bad = |key: &str, value: &str| {
            invalid.insert(InvalidSetting {
                object: w.workload.object_ref(),
                key: key.into(),
                value: value.into(),
            });
        };
        let get = |key: &str| w.reaper_annotations.get(key);
        // An override only raises the global value; one out of range is
        // reported and ignored.
        let mut duration = |key: &str, (min, max): (u64, u64), global: Duration| {
            get(key).map_or(global, |v| match parse_duration(v) {
                Ok(d) if (min..=max).contains(&d.as_secs()) => d.max(global),
                _ => {
                    bad(key, v);
                    global
                }
            })
        };
        let crash_loop_duration = duration(
            CRASHLOOP_DURATION,
            CRASH_LOOP_DURATION_RANGE,
            config.detection.crash_loop_duration,
        );
        let failing_duration = duration(
            FAILING_DURATION,
            FAILING_DURATION_RANGE,
            config.cron_jobs.failing_duration,
        );
        let mut count = |key: &str, (min, max): (u32, u32), global: u32| {
            get(key).map_or(global, |v| match v.parse::<u32>() {
                Ok(n) if (min..=max).contains(&n) => n.max(global),
                _ => {
                    bad(key, v);
                    global
                }
            })
        };
        let min_restarts = count(
            MIN_RESTARTS,
            MIN_RESTARTS_RANGE,
            config.detection.min_restarts,
        );
        let min_failed_runs = count(
            MIN_FAILED_RUNS,
            MIN_FAILED_RUNS_RANGE,
            config.cron_jobs.min_failed_runs,
        );
        let rate = &config.cron_jobs.failure_rate;
        let window_runs = count(FAILURE_WINDOW, WINDOW_RUNS_RANGE, rate.window_runs);
        let threshold = get(FAILURE_THRESHOLD).map_or(rate.threshold, |v| {
            let (min, max) = THRESHOLD_RANGE;
            match parse_percent(v) {
                Some(p) if (min..=max).contains(&p) => p.max(rate.threshold),
                _ => {
                    bad(FAILURE_THRESHOLD, v);
                    rate.threshold
                }
            }
        });
        // `off` is the only value: an annotation can turn the rule off, never on.
        let turned_off = get(FAILURE_RATE).is_some_and(|v| {
            if v != "off" {
                bad(FAILURE_RATE, v);
            }
            v == "off"
        });
        Thresholds {
            crash_loop_duration,
            min_restarts,
            failing_duration,
            min_failed_runs,
            failure_rate_enabled: rate.enabled && !turned_off,
            window_runs,
            threshold,
        }
    }
}

fn decide_workload(
    snap: &Snapshot,
    config: &Config,
    w: &WorkloadView,
    pods: Option<&WorkloadPods<'_>>,
    mode: Mode,
    thresholds: &Thresholds,
) -> Decision {
    let (record, _) = WorkloadRecord::decode(&w.reaper_annotations, w.state_label.as_deref());
    let decision = |plan, since, healthy_since, evidence| Decision {
        workload: w.workload.clone(),
        plan,
        since,
        healthy_since,
        evidence,
        run_history: None,
    };
    let has_state = record.state.is_some() || record.crashloop_since.is_some();

    match record.state {
        Some(State::Reaped | State::Reaping) => {
            let plan = held_plan(snap, w, &record, mode);
            return decision(plan, record.crashloop_since, None, None);
        }
        _ if record.restore && mode != Mode::Off => {
            return decision(Plan::RestoreIgnored, None, None, None);
        }
        _ => {}
    }
    if mode == Mode::Off || w.spec_replicas == Some(0) {
        let plan = if has_state {
            Plan::Forget
        } else {
            Plan::Nothing
        };
        return decision(plan, None, None, None);
    }

    let empty = WorkloadPods::default();
    let pods = pods.unwrap_or(&empty);
    let evidence = pods.worst().map(|(pod, c)| Evidence::Container {
        pod: pod.to_owned(),
        container: c.name.clone(),
        restarts: c.restart_count,
        last_exit: c.terminated.clone().or_else(|| c.last_terminated.clone()),
    });
    let restarts = pods.worst().map_or(0, |(_, c)| c.restart_count);

    let healthy_since = if pods.crash_looping() {
        None
    } else if has_state {
        let healthy_since = record.healthy_since.unwrap_or(snap.now);
        if snap
            .now
            .elapsed_at_least(healthy_since, config.detection.recovery_window)
        {
            return decision(Plan::Recover, None, None, None);
        }
        Some(healthy_since)
    } else {
        return decision(Plan::Nothing, None, None, None);
    };

    // FR-003: the start is kept through flapping until a full recovery.
    let since = record.crashloop_since.unwrap_or(snap.now);
    let is_candidate = snap
        .now
        .elapsed_at_least(since, thresholds.crash_loop_duration)
        && restarts >= thresholds.min_restarts;
    let plan = if is_candidate {
        candidate_plan(snap, config, w, mode)
    } else {
        Plan::Observe
    };
    decision(plan, Some(since), healthy_since, evidence)
}

/// A CronJob, decided from its Jobs (FR-028 to FR-033). Like any workload
/// once held; otherwise failing runs take the place of a crash loop. Two
/// rules make a candidate: the failing streak, and the failure rate over the
/// recorded run history (spec 003). State clears once the streak passes and
/// the window holds no failure, with no recovery window.
fn decide_cron_job(
    snap: &Snapshot,
    config: &Config,
    w: &WorkloadView,
    cron: &CronJobView,
    mode: Mode,
    thresholds: &Thresholds,
) -> Decision {
    let (record, _) = WorkloadRecord::decode(&w.reaper_annotations, w.state_label.as_deref());
    let decision = |plan, since, evidence, run_history| Decision {
        workload: w.workload.clone(),
        plan,
        since,
        healthy_since: None,
        evidence,
        run_history,
    };
    let has_state =
        record.state.is_some() || record.failing_since.is_some() || record.run_history.is_some();

    match record.state {
        // Restore keeps working with `cronJobs.enabled: false` (FR-033).
        // The history is kept to explain the reap.
        Some(State::Reaped | State::Reaping) => {
            let plan = held_plan(snap, w, &record, mode);
            return decision(plan, record.failing_since, None, record.run_history);
        }
        _ if record.restore && mode != Mode::Off => {
            return decision(Plan::RestoreIgnored, None, None, None);
        }
        _ => {}
    }
    // Suspended by someone else: never a candidate (FR-031).
    if mode == Mode::Off || !config.cron_jobs.enabled || cron.suspended {
        let plan = if has_state {
            Plan::Forget
        } else {
            Plan::Nothing
        };
        return decision(plan, None, None, None);
    }

    let streak = job_streak(cron, &record);
    if streak == JobStreak::Unknown {
        return decision(Plan::Nothing, None, None, None);
    }
    let history = if thresholds.failure_rate_enabled {
        update_history(
            record.run_history.clone(),
            cron,
            &streak,
            thresholds.window_runs,
        )
    } else {
        None
    };
    let failing = if let JobStreak::Failing(s) = streak {
        Some(s)
    } else {
        None
    };
    if failing.is_none() && history.is_none() {
        // Turning the rule off is not a recovery: drop its history quietly.
        let plan = if !has_state {
            Plan::Nothing
        } else if thresholds.failure_rate_enabled || record.failing_since.is_some() {
            Plan::Recover
        } else {
            Plan::Forget
        };
        return decision(plan, None, None, None);
    }

    let streak_met = failing.as_ref().is_some_and(|s| {
        snap.now
            .elapsed_at_least(s.since, thresholds.failing_duration)
            && s.failed_runs >= thresholds.min_failed_runs
    });
    let rate = history.as_ref().map(|h| {
        rate_met(
            h,
            thresholds.failure_rate_enabled,
            thresholds.window_runs,
            thresholds.threshold,
            thresholds.failing_duration,
            snap.now,
        )
    });
    let plan = if streak_met || rate.is_some_and(|r| r.met) {
        candidate_plan(snap, config, w, mode)
    } else {
        Plan::Observe
    };
    // FR-007: ordered by the earliest start among the rules.
    let since = [
        failing.as_ref().map(|s| s.since),
        history.as_ref().map(|h| h.since),
    ]
    .into_iter()
    .flatten()
    .min();
    let evidence = job_failures(failing, streak_met, rate, history.as_ref(), cron);
    decision(plan, since, Some(evidence), history)
}

/// The evidence for a CronJob: its failing streak, or when the streak is not
/// failing, the window's newest failure, which is never an inferred run.
fn job_failures(
    failing: Option<Streak>,
    streak_met: bool,
    rate: Option<RateEvidence>,
    history: Option<&RunHistory>,
    cron: &CronJobView,
) -> Evidence {
    if let Some(s) = failing {
        return Evidence::JobFailures {
            failed_runs: s.failed_runs,
            last_failed_job: s.last_failed_job,
            last_failure_reason: s.last_failure_reason,
            streak_since: Some(s.since),
            streak_met,
            rate,
        };
    }
    let job = history
        .and_then(|h| h.runs.iter().rev().find(|r| !r.ok))
        .and_then(|r| r.job.clone())
        .unwrap_or_default();
    let reason = cron
        .jobs
        .iter()
        .find(|j| j.name == job)
        .and_then(|j| match &j.outcome {
            JobOutcome::Failed { reason, .. } => reason.clone(),
            _ => None,
        });
    Evidence::JobFailures {
        failed_runs: 0,
        last_failed_job: job,
        last_failure_reason: reason,
        streak_since: None,
        streak_met: false,
        rate,
    }
}

fn candidate_plan(snap: &Snapshot, config: &Config, w: &WorkloadView, mode: Mode) -> Plan {
    let skip = |reason, would| Plan::Skip { reason, would };
    if w.workload.kind == WorkloadKind::DaemonSet {
        return skip(SkipReason::CannotScaleToZero, Vec::new());
    }
    let would = reap_actions(snap, w);
    // Neither an autoscaler nor available replicas apply to a CronJob.
    let scalable = w.cron_job.is_none();
    let flux_disabled = (w.flux.kustomization.is_some() && !config.flux.kustomize)
        || (w.flux.helm_release.is_some() && !config.flux.helm_release);
    let reason = if w.has_controller_owner {
        Some(SkipReason::ManagedByController)
    } else if scalable && snap.keda_targets.contains(&w.workload) {
        Some(SkipReason::ManagedByAutoscaler)
    } else if mode < Mode::Full || flux_disabled {
        Some(SkipReason::ReportOnlyScope)
    } else if scalable && w.available_replicas > 0 {
        Some(SkipReason::PartiallyAvailable)
    } else {
        None
    };
    match reason {
        Some(reason) => skip(reason, would),
        None => Plan::Reap { actions: would },
    }
}

/// A workload the reaper is reaping or has reaped (research R6; 002
/// data-model.md "`RestoreKind` and when each applies"). The first match wins:
///
/// 1. its namespace left scope (`out_of_scope` passes `Mode::Off`), or
/// 2. `mode=off` on it or its namespace: `opted-out`;
/// 3. `restore=true`: `requested`;
/// 4. reaped, and a person resumed the HelmRelease the reaper suspended for
///    it: `manual-resume`;
/// 5. reaped, and a person resumed the CronJob: `manual-resume`;
/// 6. reaped, and a person scaled it up: `manual-scale-up`.
///
/// Otherwise finish an interrupted reap, or keep holding it.
fn held_plan(snap: &Snapshot, w: &WorkloadView, record: &WorkloadRecord, mode: Mode) -> Plan {
    let reaped = record.state == Some(State::Reaped);
    let kind = if mode == Mode::Off {
        Some(RestoreKind::OptedOut)
    } else if record.restore {
        Some(RestoreKind::Requested)
    } else if reaped && release_resumed(snap, w, record) {
        Some(RestoreKind::ManualResume)
    } else if reaped && brought_back(w) {
        Some(if w.cron_job.is_some() {
            RestoreKind::ManualResume
        } else {
            RestoreKind::ManualScaleUp
        })
    } else {
        None
    };
    if let Some(kind) = kind {
        return Plan::Restore {
            kind,
            actions: restore_actions(snap, w, record, kind),
        };
    }
    if reaped {
        return Plan::Held;
    }
    Plan::Reap {
        actions: remaining_reap_actions(snap, w, record),
    }
}

/// The HelmRelease this workload holds: as recorded, or as found in its
/// `held-for` when the reap stopped before recording it.
fn held_helm_release(snap: &Snapshot, w: &WorkloadView, record: &WorkloadRecord) -> Option<NsName> {
    record.helm_release.clone().or_else(|| {
        let hr = w.flux.helm_release.as_ref()?;
        let (hold, _) = HelmReleaseHold::decode(&snap.helm_releases.get(hr)?.reaper_annotations);
        hold.held_for.contains(&w.workload).then(|| hr.clone())
    })
}

/// Rule #4 (002 FR-009): the HelmRelease this workload is held through is in
/// the snapshot, the reaper suspended it (`suspended-by-reaper=true`), and it
/// is no longer suspended. A release someone else had suspended before the
/// reap does not count. Seen once per cycle: a resume and a new suspend
/// between two cycles is not seen.
fn release_resumed(snap: &Snapshot, w: &WorkloadView, record: &WorkloadRecord) -> bool {
    held_helm_release(snap, w, record)
        .and_then(|hr| snap.helm_releases.get(&hr))
        .is_some_and(|view| {
            let (hold, _) = HelmReleaseHold::decode(&view.reaper_annotations);
            hold.suspended_by_reaper == Some(true) && !view.suspended
        })
}

fn restore_actions(
    snap: &Snapshot,
    w: &WorkloadView,
    record: &WorkloadRecord,
    kind: RestoreKind,
) -> Vec<Action> {
    let mut actions = Vec::new();
    if let Some(hr) = held_helm_release(snap, w, record) {
        let view = snap.helm_releases.get(&hr);
        let (hold, _) = view
            .map(|v| HelmReleaseHold::decode(&v.reaper_annotations))
            .unwrap_or_default();
        let last = hold.held_for.iter().all(|h| *h == w.workload);
        // Only what the reaper suspended, and only while it still is: a
        // release a person resumed is already running (`hold::release`
        // makes the same check when it writes).
        let suspended = view.is_some_and(|v| v.suspended);
        actions.push(Action::ReleaseHelmRelease {
            helm_release: hr,
            resume: last && suspended && hold.suspended_by_reaper == Some(true),
            restore_reconcile: hold.previous_reconcile.filter(|_| last),
        });
    }
    // A person who scaled it up chose the replicas; otherwise put back what
    // was there (1 if that was never recorded). Already there means a crash
    // between restore steps: just finish. Likewise for a CronJob a person
    // already resumed.
    if let Some(cron) = &w.cron_job {
        if cron.suspended {
            actions.push(Action::ResumeCronJob);
        }
    } else {
        let target = record.original_replicas.unwrap_or(1);
        if kind != RestoreKind::ManualScaleUp && w.spec_replicas != Some(target) {
            actions.push(Action::ScaleTo { replicas: target });
        }
    }
    actions.push(Action::ClearReaperState {
        restore_reconcile: record.previous_reconcile.clone(),
        last_counted_job: w.cron_job.as_ref().and_then(newest_failed_job),
    });
    actions
}

/// The most recent failed Job, which a restore marks as counted: the
/// failing streak starts again from zero (spec.md US5 scenario 5).
fn newest_failed_job(cron: &CronJobView) -> Option<String> {
    cron.jobs
        .iter()
        .filter(|j| j.failed())
        .filter_map(|j| Some((j.finished_at()?, &j.name)))
        .max()
        .map(|(_, name)| name.clone())
}

/// The steps of an interrupted reap that are not visible as done yet
/// (FR-016). Recording is done: the state label says `reaping`.
fn remaining_reap_actions(
    snap: &Snapshot,
    w: &WorkloadView,
    record: &WorkloadRecord,
) -> Vec<Action> {
    let mut actions = Vec::new();
    if w.flux.kustomization.is_some()
        && w.flux.reconcile_annotation.as_deref() != Some(FLUX_RECONCILE_DISABLED)
    {
        actions.push(Action::DisableKustomizeReconcile(w.workload.object_ref()));
    }
    if let Some(hr) = &w.flux.helm_release {
        let in_hold = snap.helm_releases.get(hr).is_some_and(|v| {
            HelmReleaseHold::decode(&v.reaper_annotations)
                .0
                .held_for
                .contains(&w.workload)
        });
        if !in_hold || record.helm_release.is_none() {
            actions.push(hold_action(snap, hr));
        }
    }
    match &w.cron_job {
        Some(cron) if !cron.suspended => actions.push(Action::SuspendCronJob),
        None if w.spec_replicas != Some(0) => actions.push(Action::ScaleTo { replicas: 0 }),
        _ => {}
    }
    actions.push(Action::MarkReaped);
    actions
}

/// The hold as it will be placed: suspend only what nobody suspended yet.
fn hold_action(snap: &Snapshot, hr: &NsName) -> Action {
    let view = snap.helm_releases.get(hr);
    let (hold, _) = view
        .map(|v| HelmReleaseHold::decode(&v.reaper_annotations))
        .unwrap_or_default();
    Action::HoldHelmRelease {
        helm_release: hr.clone(),
        suspend: hold.suspended_by_reaper.is_none() && !view.is_some_and(|v| v.suspended),
        disable_reconcile: view.is_some_and(|v| {
            v.kustomization.is_some()
                && v.reconcile_annotation.as_deref() != Some(FLUX_RECONCILE_DISABLED)
        }),
    }
}

/// Every reap step for a workload nothing has been done to yet (research R6):
/// record, block Flux, scale (or suspend), mark.
fn reap_actions(snap: &Snapshot, w: &WorkloadView) -> Vec<Action> {
    let mut actions = vec![Action::RecordReap {
        original_replicas: w.spec_replicas,
    }];
    if w.flux.kustomization.is_some() {
        actions.push(Action::DisableKustomizeReconcile(w.workload.object_ref()));
    }
    if let Some(hr) = &w.flux.helm_release {
        actions.push(hold_action(snap, hr));
    }
    actions.push(if w.cron_job.is_some() {
        Action::SuspendCronJob
    } else {
        Action::ScaleTo { replicas: 0 }
    });
    actions.push(Action::MarkReaped);
    actions
}

/// The published plans of a cycle, keyed by workload (used by tests and the
/// equivalence harness).
#[must_use]
pub fn published_plans(plan: &CyclePlan) -> BTreeMap<String, String> {
    plan.decisions
        .iter()
        .filter_map(|d| Some((d.workload.to_string(), d.plan.published_json()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NsName, WorkloadRef};
    use crate::record::{
        CRASHLOOP_SINCE, FAILING_SINCE, HEALTHY_SINCE, HELMRELEASE, ORIGINAL_REPLICAS,
        PREVIOUS_RECONCILE, PreviousReconcile, RUN_HISTORY,
    };
    use crate::testing::{
        NOW, add_namespace, add_pod, crashlooping_container, cron_job, cron_mut, deployment,
        failed_job, flaky_cron, healthy_container, helm_release, mins_ago, snapshot, succeeded_job,
        workload,
    };

    /// A Deployment whose pod crash-loops with `restarts`, first observed
    /// `minutes` ago (`None`: never observed).
    fn looping(name: &str, restarts: u32, minutes: Option<i64>) -> (Snapshot, WorkloadView) {
        let mut snap = snapshot();
        let mut d = deployment("app", name);
        if let Some(m) = minutes {
            d.reaper_annotations
                .insert(CRASHLOOP_SINCE.into(), mins_ago(m).to_string());
            d.state_label = Some("observed".into());
        }
        add_pod(
            &mut snap,
            &d.workload,
            &format!("{name}-1"),
            vec![crashlooping_container("api", restarts)],
        );
        (snap, d)
    }

    fn run(mut snap: Snapshot, workloads: Vec<WorkloadView>) -> CyclePlan {
        snap.workloads = workloads;
        decide(&snap, &Config::default())
    }

    fn only(plan: &CyclePlan) -> &Decision {
        assert_eq!(plan.decisions.len(), 1, "{:?}", plan.decisions);
        &plan.decisions[0]
    }

    // US1 scenario 3

    #[test]
    fn first_sighting_is_observed_from_now() {
        let (snap, d) = looping("web", 40, None);
        let p = run(snap, vec![d]);
        let d = only(&p);
        assert_eq!(d.plan, Plan::Observe);
        assert_eq!(d.since, Some(NOW));
    }

    #[test]
    fn under_duration_is_observed() {
        let (snap, d) = looping("web", 40, Some(29));
        assert_eq!(only(&run(snap, vec![d])).plan, Plan::Observe);
    }

    #[test]
    fn under_restarts_is_observed() {
        let (snap, d) = looping("web", 9, Some(45));
        let p = run(snap, vec![d]);
        assert_eq!(only(&p).plan, Plan::Observe);
        assert_eq!(only(&p).since, Some(mins_ago(45)));
    }

    // US1 scenario 1

    #[test]
    fn both_thresholds_met_is_a_candidate_with_the_full_reap() {
        let (snap, d) = looping("web", 10, Some(30));
        let p = run(snap, vec![d]);
        assert_eq!(
            only(&p).plan,
            Plan::Reap {
                actions: vec![
                    Action::RecordReap {
                        original_replicas: Some(2)
                    },
                    Action::ScaleTo { replicas: 0 },
                    Action::MarkReaped,
                ]
            }
        );
        assert_eq!(
            only(&p).evidence,
            Some(Evidence::Container {
                pod: "web-1".into(),
                container: "api".into(),
                restarts: 10,
                last_exit: Some(crate::model::Termination {
                    exit_code: 1,
                    reason: "Error".into()
                }),
            })
        );
    }

    #[test]
    fn would_actions_cover_kustomize_and_helm() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        d.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        d.flux.helm_release = Some(NsName::new("helm", "web"));
        let mut hr = helm_release("helm", "web");
        hr.suspended = true;
        hr.kustomization = Some(NsName::new("flux-system", "releases"));
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        let p = run(snap, vec![d]);
        let Plan::Reap { actions } = &only(&p).plan else {
            panic!("{:?}", only(&p).plan)
        };
        assert_eq!(
            actions[1..3],
            [
                Action::DisableKustomizeReconcile(
                    WorkloadRef::new(WorkloadKind::Deployment, "app", "web").object_ref()
                ),
                Action::HoldHelmRelease {
                    helm_release: NsName::new("helm", "web"),
                    suspend: false,
                    disable_reconcile: true,
                },
            ]
        );
    }

    #[test]
    fn overrides_only_raise_thresholds() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.reaper_annotations
            .insert(CRASHLOOP_DURATION.into(), "2h".into());
        let p = run(snap, vec![d]);
        assert_eq!(only(&p).plan, Plan::Observe);

        let (snap, mut d) = looping("web", 12, Some(60));
        d.reaper_annotations
            .insert(CRASHLOOP_DURATION.into(), "5m".into());
        d.reaper_annotations.insert(MIN_RESTARTS.into(), "2".into());
        assert!(matches!(only(&run(snap, vec![d])).plan, Plan::Reap { .. }));

        let (snap, mut d) = looping("web", 12, Some(60));
        d.reaper_annotations
            .insert(MIN_RESTARTS.into(), "20".into());
        assert_eq!(only(&run(snap, vec![d])).plan, Plan::Observe);
    }

    #[test]
    fn invalid_overrides_are_reported_and_ignored() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.reaper_annotations
            .insert(CRASHLOOP_DURATION.into(), "1m".into());
        d.reaper_annotations
            .insert(MIN_RESTARTS.into(), "lots".into());
        let p = run(snap, vec![d]);
        assert!(matches!(only(&p).plan, Plan::Reap { .. }));
        let keys: Vec<&str> = p.invalid_settings.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, [CRASHLOOP_DURATION, MIN_RESTARTS]);
    }

    // FR-003 and US1 scenario 4

    #[test]
    fn flapping_keeps_the_crash_loop_start() {
        let (mut snap, mut d) = looping("web", 12, Some(20));
        d.reaper_annotations
            .insert(HEALTHY_SINCE.into(), mins_ago(5).to_string());
        snap.pods[0].containers = vec![crashlooping_container("api", 13)];
        let p = run(snap, vec![d]);
        assert_eq!(only(&p).since, Some(mins_ago(20)));
        assert_eq!(only(&p).healthy_since, None);
    }

    #[test]
    fn healthy_inside_the_recovery_window_keeps_state() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        d.state_label = Some("candidate".into());
        snap.pods[0].containers = vec![crate::model::ContainerView {
            restart_count: 12,
            ..healthy_container("api")
        }];
        let p = run(snap.clone(), vec![d.clone()]);
        let first = only(&p);
        assert!(matches!(first.plan, Plan::Reap { .. }), "{:?}", first.plan);
        assert_eq!(first.since, Some(mins_ago(60)));
        assert_eq!(first.healthy_since, Some(NOW));

        d.reaper_annotations
            .insert(HEALTHY_SINCE.into(), mins_ago(9).to_string());
        assert_eq!(
            only(&run(snap.clone(), vec![d.clone()])).healthy_since,
            Some(mins_ago(9))
        );

        d.reaper_annotations
            .insert(HEALTHY_SINCE.into(), mins_ago(10).to_string());
        assert_eq!(only(&run(snap, vec![d])).plan, Plan::Recover);
    }

    #[test]
    fn healthy_workloads_without_state_decide_nothing() {
        let mut snap = snapshot();
        let d = deployment("app", "web");
        add_pod(
            &mut snap,
            &d.workload,
            "web-1",
            vec![healthy_container("api")],
        );
        assert_eq!(run(snap, vec![d]).decisions, Vec::<Decision>::new());
    }

    #[test]
    fn scaled_to_zero_by_someone_else_is_forgotten() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        snap.pods.clear();
        d.spec_replicas = Some(0);
        assert_eq!(only(&run(snap, vec![d])).plan, Plan::Forget);
    }

    // Skips

    #[test]
    fn daemonset_cannot_scale_to_zero() {
        let mut snap = snapshot();
        let mut ds = workload(WorkloadKind::DaemonSet, "app", "agent");
        ds.reaper_annotations
            .insert(CRASHLOOP_SINCE.into(), mins_ago(60).to_string());
        snap.pods.push(crate::model::PodView {
            namespace: "app".into(),
            name: "agent-x".into(),
            owner: Some(crate::model::OwnerRef {
                kind: "DaemonSet".into(),
                name: "agent".into(),
                controller: true,
            }),
            containers: vec![crashlooping_container("agent", 30)],
        });
        assert_eq!(
            only(&run(snap, vec![ds])).plan,
            Plan::Skip {
                reason: SkipReason::CannotScaleToZero,
                would: vec![]
            }
        );
    }

    fn skip_reason(p: &CyclePlan) -> Option<SkipReason> {
        match &only(p).plan {
            Plan::Skip { reason, would } => {
                assert_ne!(would, &Vec::<Action>::new());
                Some(*reason)
            }
            _ => None,
        }
    }

    #[test]
    fn partially_available_is_skipped() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.available_replicas = 1;
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::PartiallyAvailable)
        );
    }

    #[test]
    fn controller_owned_is_skipped() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.has_controller_owner = true;
        d.available_replicas = 1;
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::ManagedByController)
        );
    }

    #[test]
    fn keda_target_is_skipped() {
        let (mut snap, d) = looping("web", 12, Some(60));
        snap.keda_targets.insert(d.workload.clone());
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::ManagedByAutoscaler)
        );
    }

    #[test]
    fn report_label_on_namespace_or_workload_is_report_only() {
        let (mut snap, d) = looping("web", 12, Some(60));
        add_namespace(&mut snap, "app", Some("report"));
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::ReportOnlyScope)
        );

        let (snap, mut d) = looping("web", 12, Some(60));
        d.mode_annotation = Some("report".into());
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::ReportOnlyScope)
        );
    }

    #[test]
    fn workload_label_wins_over_its_annotation() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.mode_label = Some("report".into());
        d.mode_annotation = Some("off".into());
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::ReportOnlyScope)
        );
    }

    #[test]
    fn a_label_never_raises_the_mode() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        add_namespace(&mut snap, "app", Some("report"));
        d.mode_label = Some("full".into());
        assert_eq!(
            skip_reason(&run(snap, vec![d])),
            Some(SkipReason::ReportOnlyScope)
        );

        let (mut snap, mut d) = looping("web", 12, None);
        add_namespace(&mut snap, "app", Some("off"));
        d.mode_label = Some("full".into());
        assert_eq!(run(snap, vec![d]).decisions, Vec::<Decision>::new());
    }

    #[test]
    fn invalid_mode_label_is_reported_and_ignored() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        add_namespace(&mut snap, "app", Some("full"));
        d.mode_label = Some("yes".into());
        let p = run(snap, vec![d]);
        assert!(matches!(only(&p).plan, Plan::Reap { .. }));
        let values: Vec<&str> = p
            .invalid_settings
            .iter()
            .map(|s| s.value.as_str())
            .collect();
        assert_eq!(values, ["yes", "full"]);
    }

    #[test]
    fn flux_toggle_off_makes_flux_workloads_report_only() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        d.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        snap.workloads = vec![d];
        let mut config = Config::default();
        config.flux.kustomize = false;
        let p = decide(&snap, &config);
        assert_eq!(skip_reason(&p), Some(SkipReason::ReportOnlyScope));
    }

    #[test]
    fn mode_off_forgets_existing_state_and_ignores_new() {
        let (mut snap, d) = looping("web", 12, Some(60));
        add_namespace(&mut snap, "app", Some("off"));
        assert_eq!(only(&run(snap.clone(), vec![d])).plan, Plan::Forget);
        let (_, fresh) = looping("web", 12, None);
        assert_eq!(run(snap, vec![fresh]).decisions, Vec::<Decision>::new());
    }

    // Scope (FR-011)

    #[test]
    fn excluded_own_and_unincluded_namespaces_decide_nothing() {
        let mut snap = snapshot();
        let mut workloads = Vec::new();
        for ns in ["kube-system", "k8s-reaper", "other", "shop-prod"] {
            let mut d = deployment(ns, "web");
            d.reaper_annotations
                .insert(CRASHLOOP_SINCE.into(), mins_ago(60).to_string());
            add_pod(
                &mut snap,
                &d.workload,
                "web-1",
                vec![crashlooping_container("api", 50)],
            );
            workloads.push(d);
        }
        snap.workloads = workloads;
        let mut config = Config::default();
        config.scope.include_namespaces = vec!["shop-*".into(), "k8s-*".into()];
        let p = decide(&snap, &config);
        let names: Vec<String> = p.decisions.iter().map(|d| d.workload.to_string()).collect();
        assert_eq!(names, ["Deployment/shop-prod/web"]);
    }

    #[test]
    fn unsupported_pods_are_reported() {
        let mut snap = snapshot();
        snap.pods.push(crate::model::PodView {
            namespace: "app".into(),
            name: "bare".into(),
            owner: None,
            containers: vec![crashlooping_container("api", 50)],
        });
        let p = decide(&snap, &Config::default());
        assert_eq!(p.unsupported.len(), 1);
        assert_eq!(p.decisions, Vec::<Decision>::new());
    }

    // FR-017 ordering

    #[test]
    fn longest_crash_looping_first_then_name() {
        let (mut snap, a) = looping("b-web", 12, Some(40));
        let (s2, b) = looping("a-web", 12, Some(40));
        let (s3, c) = looping("z-web", 12, Some(90));
        snap.pods.extend(s2.pods);
        snap.pods.extend(s3.pods);
        snap.replica_sets.extend(s2.replica_sets);
        snap.replica_sets.extend(s3.replica_sets);
        let p = run(snap, vec![a, b, c]);
        let names: Vec<&str> = p
            .decisions
            .iter()
            .map(|d| d.workload.name.as_str())
            .collect();
        assert_eq!(names, ["z-web", "a-web", "b-web"]);
    }

    #[test]
    fn published_plans_are_keyed_by_workload() {
        let (snap, d) = looping("web", 12, Some(60));
        let plans = published_plans(&run(snap, vec![d]));
        assert_eq!(plans.keys().collect::<Vec<_>>(), ["Deployment/app/web"]);
    }

    // US2: limits, resume, Flux (T040)

    fn reap_of(p: &CyclePlan, name: &str) -> Plan {
        p.decisions
            .iter()
            .find(|d| d.workload.name == name)
            .map_or_else(|| panic!("{name} not decided"), |d| d.plan.clone())
    }

    fn skip_of(plan: &Plan) -> Option<SkipReason> {
        match plan {
            Plan::Skip { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// A workload the reaper holds: `state` label, 3 original replicas.
    fn held(name: &str, state: &str, replicas: u32) -> WorkloadView {
        let mut d = deployment("app", name);
        d.state_label = Some(state.into());
        d.spec_replicas = Some(replicas);
        d.reaper_annotations
            .insert(CRASHLOOP_SINCE.into(), mins_ago(90).to_string());
        d.reaper_annotations
            .insert(ORIGINAL_REPLICAS.into(), "3".into());
        d
    }

    fn add_looping(snap: &mut Snapshot, name: &str, minutes: i64) -> WorkloadView {
        let (one, d) = looping(name, 20, Some(minutes));
        snap.pods.extend(one.pods);
        snap.replica_sets.extend(one.replica_sets);
        d
    }

    #[test]
    fn one_new_reap_per_cycle_longest_looping_first() {
        let mut snap = snapshot();
        let ws = vec![
            add_looping(&mut snap, "b", 40),
            add_looping(&mut snap, "c", 90),
            add_looping(&mut snap, "a", 40),
        ];
        let p = run(snap, ws);
        let order: Vec<_> = p
            .decisions
            .iter()
            .map(|d| d.workload.name.as_str())
            .collect();
        assert_eq!(order, ["c", "a", "b"]);
        assert!(matches!(reap_of(&p, "c"), Plan::Reap { .. }));
        assert_eq!(skip_of(&reap_of(&p, "a")), Some(SkipReason::RateLimited));
        assert_eq!(skip_of(&reap_of(&p, "b")), Some(SkipReason::RateLimited));
        // `would` is kept for the report.
        let Plan::Skip { would, .. } = reap_of(&p, "b") else {
            unreachable!()
        };
        assert_eq!(would.last(), Some(&Action::MarkReaped));
    }

    #[test]
    fn ceiling_counts_reaping_and_reaped() {
        let mut snap = snapshot();
        let mut config = Config::default();
        config.limits.max_reaps_per_cycle = 5;
        config.limits.max_reaped = 2;
        snap.workloads = vec![
            held("old", "reaped", 0),
            add_looping(&mut snap, "x", 60),
            add_looping(&mut snap, "y", 50),
        ];
        let p = decide(&snap, &config);
        assert!(matches!(reap_of(&p, "x"), Plan::Reap { .. }));
        assert_eq!(
            skip_of(&reap_of(&p, "y")),
            Some(SkipReason::ReapCeilingReached)
        );
    }

    #[test]
    fn zero_per_cycle_reports_everything() {
        let mut snap = snapshot();
        let mut config = Config::default();
        config.limits.max_reaps_per_cycle = 0;
        snap.workloads = vec![add_looping(&mut snap, "x", 60)];
        let p = decide(&snap, &config);
        assert_eq!(skip_of(&reap_of(&p, "x")), Some(SkipReason::RateLimited));
    }

    #[test]
    fn interrupted_reap_resumes_past_the_limits() {
        let mut snap = snapshot();
        let mut config = Config::default();
        config.limits.max_reaped = 0;
        let mut w = held("web", "reaping", 3);
        w.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        w.flux.reconcile_annotation = Some("disabled".into());
        snap.workloads = vec![w];
        let p = decide(&snap, &config);
        // Replicas > 0 while reaping is the reap continuing, not a restore.
        assert_eq!(
            reap_of(&p, "web"),
            Plan::Reap {
                actions: vec![Action::ScaleTo { replicas: 0 }, Action::MarkReaped]
            }
        );

        snap.workloads[0].spec_replicas = Some(0);
        let p = decide(&snap, &config);
        assert_eq!(
            reap_of(&p, "web"),
            Plan::Reap {
                actions: vec![Action::MarkReaped]
            }
        );
    }

    #[test]
    fn interrupted_reap_places_a_missing_hold() {
        let mut snap = snapshot();
        let mut w = held("web", "reaping", 3);
        w.flux.helm_release = Some(NsName::new("helm", "web"));
        snap.helm_releases
            .insert(NsName::new("helm", "web"), helm_release("helm", "web"));
        snap.workloads = vec![w];
        let Plan::Reap { actions } = reap_of(&run(snap.clone(), snap.workloads.clone()), "web")
        else {
            panic!()
        };
        assert_eq!(
            actions[0],
            Action::HoldHelmRelease {
                helm_release: NsName::new("helm", "web"),
                suspend: true,
                disable_reconcile: false,
            }
        );
    }

    #[test]
    fn flux_toggle_off_is_report_only() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.flux.helm_release = Some(NsName::new("helm", "web"));
        let mut config = Config::default();
        config.flux.helm_release = false;
        let mut snap = snap;
        snap.workloads = vec![d];
        let p = decide(&snap, &config);
        assert_eq!(skip_of(&only(&p).plan), Some(SkipReason::ReportOnlyScope));
    }

    #[test]
    fn a_release_another_workload_holds_is_not_suspended_again() {
        let (mut snap, mut d) = looping("web", 12, Some(60));
        d.flux.helm_release = Some(NsName::new("helm", "app"));
        let mut hr = helm_release("helm", "app");
        hr.suspended = true;
        hr.reaper_annotations.insert(
            crate::record::HELD_FOR.into(),
            r#"["StatefulSet/helm/db"]"#.into(),
        );
        hr.reaper_annotations
            .insert(crate::record::SUSPENDED_BY_REAPER.into(), "true".into());
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        let p = run(snap, vec![d]);
        let Plan::Reap { actions } = &only(&p).plan else {
            panic!()
        };
        assert_eq!(
            actions[1],
            Action::HoldHelmRelease {
                helm_release: NsName::new("helm", "app"),
                suspend: false,
                disable_reconcile: false,
            }
        );
    }

    // US3: restore (T050)

    fn restore_of(p: &CyclePlan, name: &str) -> (RestoreKind, Vec<Action>) {
        match reap_of(p, name) {
            Plan::Restore { kind, actions } => (kind, actions),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reaped_and_unchanged_is_held() {
        let snap = snapshot();
        assert_eq!(
            reap_of(&run(snap, vec![held("web", "reaped", 0)]), "web"),
            Plan::Held
        );
    }

    #[test]
    fn restore_annotation_scales_back_and_clears() {
        let mut w = held("web", "reaped", 0);
        w.reaper_annotations.insert(RESTORE.into(), "yes".into());
        w.reaper_annotations
            .insert(PREVIOUS_RECONCILE.into(), "<none>".into());
        let (kind, actions) = restore_of(&run(snapshot(), vec![w]), "web");
        assert_eq!(kind, RestoreKind::Requested);
        assert_eq!(
            actions,
            [
                Action::ScaleTo { replicas: 3 },
                Action::ClearReaperState {
                    restore_reconcile: Some(PreviousReconcile::Absent),
                    last_counted_job: None,
                },
            ]
        );
    }

    #[test]
    fn manual_scale_up_keeps_the_persons_replicas() {
        let (kind, actions) = restore_of(&run(snapshot(), vec![held("web", "reaped", 5)]), "web");
        assert_eq!(kind, RestoreKind::ManualScaleUp);
        assert_eq!(
            actions,
            [Action::ClearReaperState {
                restore_reconcile: None,
                last_counted_job: None,
            }]
        );
    }

    #[test]
    fn restore_interrupted_after_scaling_just_finishes() {
        let mut w = held("web", "reaped", 3);
        w.reaper_annotations.insert(RESTORE.into(), "1".into());
        let (_, actions) = restore_of(&run(snapshot(), vec![w]), "web");
        assert_eq!(
            actions,
            [Action::ClearReaperState {
                restore_reconcile: None,
                last_counted_job: None,
            }]
        );
    }

    #[test]
    fn mode_off_on_a_reaped_workload_restores_it() {
        let mut w = held("web", "reaped", 0);
        w.mode_label = Some("off".into());
        let (kind, _) = restore_of(&run(snapshot(), vec![w]), "web");
        assert_eq!(kind, RestoreKind::OptedOut);
    }

    #[test]
    fn restore_on_a_workload_not_reaped_is_ignored() {
        let (snap, mut d) = looping("web", 12, Some(60));
        d.reaper_annotations.insert(RESTORE.into(), "1".into());
        assert_eq!(only(&run(snap, vec![d])).plan, Plan::RestoreIgnored);
    }

    #[test]
    fn shared_release_is_only_resumed_by_the_last_restore() {
        let mut snap = snapshot();
        let mut hr = helm_release("helm", "app");
        hr.suspended = true;
        hr.reaper_annotations.insert(
            crate::record::HELD_FOR.into(),
            r#"["Deployment/app/db","Deployment/app/web"]"#.into(),
        );
        hr.reaper_annotations
            .insert(crate::record::SUSPENDED_BY_REAPER.into(), "true".into());
        hr.reaper_annotations
            .insert(PREVIOUS_RECONCILE.into(), "enabled".into());
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        let mut web = held("web", "reaped", 0);
        web.reaper_annotations.insert(RESTORE.into(), "1".into());
        web.reaper_annotations
            .insert(HELMRELEASE.into(), "helm/app".into());
        let db = held("db", "reaped", 0);
        snap.workloads = vec![web, db];
        let p = decide(&snap, &Config::default());
        let (_, actions) = restore_of(&p, "web");
        assert_eq!(
            actions[0],
            Action::ReleaseHelmRelease {
                helm_release: NsName::new("helm", "app"),
                resume: false,
                restore_reconcile: None,
            }
        );

        // Once db is the only one left, its restore resumes the release.
        let hr = snap
            .helm_releases
            .get_mut(&NsName::new("helm", "app"))
            .unwrap();
        hr.reaper_annotations.insert(
            crate::record::HELD_FOR.into(),
            r#"["Deployment/app/db"]"#.into(),
        );
        snap.workloads[1]
            .reaper_annotations
            .insert(RESTORE.into(), "1".into());
        snap.workloads[1]
            .reaper_annotations
            .insert(HELMRELEASE.into(), "helm/app".into());
        let (_, actions) = restore_of(&decide(&snap, &Config::default()), "db");
        assert_eq!(
            actions[0],
            Action::ReleaseHelmRelease {
                helm_release: NsName::new("helm", "app"),
                resume: true,
                restore_reconcile: Some(PreviousReconcile::Value("enabled".into())),
            }
        );
    }

    #[test]
    fn holds_for_missing_or_unheld_workloads_are_stale() {
        let mut snap = snapshot();
        let mut hr = helm_release("helm", "app");
        hr.reaper_annotations.insert(
            crate::record::HELD_FOR.into(),
            r#"["Deployment/app/gone","Deployment/app/held","Deployment/app/recreated"]"#.into(),
        );
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        snap.workloads = vec![held("held", "reaped", 0), deployment("app", "recreated")];
        let p = decide(&snap, &Config::default());
        let stale: Vec<_> = p
            .stale_holds
            .iter()
            .map(|s| s.workload.name.as_str())
            .collect();
        assert_eq!(stale, ["gone", "recreated"]);
    }

    #[test]
    fn restore_or_scale_up_of_a_reaped_workload_wants_a_cycle_now() {
        let mut w = deployment("app", "web");
        assert!(!wants_immediate_cycle(&w));
        w.reaper_annotations.insert(RESTORE.into(), String::new());
        assert!(!wants_immediate_cycle(&w));
        w.reaper_annotations.insert(RESTORE.into(), "true".into());
        assert!(wants_immediate_cycle(&w));

        let mut w = deployment("app", "web");
        w.state_label = Some("reaped".into());
        w.spec_replicas = Some(0);
        assert!(!wants_immediate_cycle(&w));
        w.spec_replicas = Some(2);
        assert!(wants_immediate_cycle(&w));
        w.state_label = Some("reaping".into());
        assert!(!wants_immediate_cycle(&w));
    }

    // 002 US3: a manual `flux resume` is a restore (T136, FR-009)

    /// A Deployment in `app` held through HelmRelease helm/app.
    fn held_through(name: &str, state: &str, replicas: u32) -> WorkloadView {
        let mut w = held(name, state, replicas);
        w.flux.helm_release = Some(NsName::new("helm", "app"));
        w.reaper_annotations
            .insert(HELMRELEASE.into(), "helm/app".into());
        w
    }

    /// HelmRelease helm/app held for `held_for` (Deployments in `app`), with
    /// the given `suspended-by-reaper` value and current `spec.suspend`.
    fn add_release(snap: &mut Snapshot, held_for: &[&str], by_reaper: &str, suspended: bool) {
        let mut hr = helm_release("helm", "app");
        hr.suspended = suspended;
        let list: Vec<String> = held_for
            .iter()
            .map(|n| format!("Deployment/app/{n}"))
            .collect();
        hr.reaper_annotations.insert(
            crate::record::HELD_FOR.into(),
            serde_json::to_string(&list).unwrap(),
        );
        hr.reaper_annotations
            .insert(crate::record::SUSPENDED_BY_REAPER.into(), by_reaper.into());
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
    }

    /// Every restore in the cycle as `{workload: {restore, actions}}` JSON
    /// (object keys sorted).
    fn restores_json(p: &CyclePlan) -> String {
        let all: BTreeMap<String, serde_json::Value> = p
            .decisions
            .iter()
            .map(|d| match &d.plan {
                Plan::Restore { kind, actions } => (
                    d.workload.to_string(),
                    serde_json::json!({ "restore": kind, "actions": actions }),
                ),
                other => panic!("{}: {other:?}", d.workload),
            })
            .collect();
        serde_json::to_string(&all).unwrap()
    }

    #[test]
    fn a_resumed_release_restores_every_workload_held_through_it() {
        let mut snap = snapshot();
        add_release(&mut snap, &["db", "web"], "true", false);
        snap.workloads = vec![
            held_through("web", "reaped", 0),
            held_through("db", "reaped", 0),
        ];
        let p = decide(&snap, &Config::default());
        assert_eq!(p.decisions.len(), 2, "{:?}", p.decisions);
        for name in ["db", "web"] {
            let (kind, actions) = restore_of(&p, name);
            assert_eq!(kind, RestoreKind::ManualResume, "{name}");
            // The release is already running: drop the workload from
            // `held-for`, never resume it.
            assert_eq!(
                actions,
                [
                    Action::ReleaseHelmRelease {
                        helm_release: NsName::new("helm", "app"),
                        resume: false,
                        restore_reconcile: None,
                    },
                    Action::ScaleTo { replicas: 3 },
                    Action::ClearReaperState {
                        restore_reconcile: None,
                        last_counted_job: None,
                    },
                ],
                "{name}"
            );
        }
        insta::assert_snapshot!(restores_json(&p), @r#"{"Deployment/app/db":{"actions":[{"releaseHelmRelease":{"helmRelease":"helm/app","restoreReconcile":null,"resume":false}},{"scaleTo":{"replicas":3}},{"clearReaperState":{"restoreReconcile":null}}],"restore":"manual-resume"},"Deployment/app/web":{"actions":[{"releaseHelmRelease":{"helmRelease":"helm/app","restoreReconcile":null,"resume":false}},{"scaleTo":{"replicas":3}},{"clearReaperState":{"restoreReconcile":null}}],"restore":"manual-resume"}}"#);
    }

    #[test]
    fn the_last_workload_of_a_resumed_release_releases_it_without_a_resume() {
        let mut snap = snapshot();
        add_release(&mut snap, &["web"], "true", false);
        snap.helm_releases
            .get_mut(&NsName::new("helm", "app"))
            .unwrap()
            .reaper_annotations
            .insert(PREVIOUS_RECONCILE.into(), "enabled".into());
        // Flux already brought it back up: the person's replicas stay.
        snap.workloads = vec![held_through("web", "reaped", 2)];
        let (kind, actions) = restore_of(&decide(&snap, &Config::default()), "web");
        assert_eq!(kind, RestoreKind::ManualResume);
        assert_eq!(
            actions[0],
            Action::ReleaseHelmRelease {
                helm_release: NsName::new("helm", "app"),
                resume: false,
                restore_reconcile: Some(PreviousReconcile::Value("enabled".into())),
            }
        );
    }

    #[test]
    fn resuming_a_release_someone_else_suspended_is_not_a_restore() {
        let mut snap = snapshot();
        add_release(&mut snap, &["web"], "false", false);
        snap.workloads = vec![held_through("web", "reaped", 0)];
        assert_eq!(
            reap_of(&decide(&snap, &Config::default()), "web"),
            Plan::Held
        );
    }

    #[test]
    fn a_reap_under_way_continues_while_the_release_runs() {
        let mut snap = snapshot();
        add_release(&mut snap, &["web"], "true", false);
        snap.workloads = vec![held_through("web", "reaping", 3)];
        assert_eq!(
            reap_of(&decide(&snap, &Config::default()), "web"),
            Plan::Reap {
                actions: vec![Action::ScaleTo { replicas: 0 }, Action::MarkReaped],
            }
        );
    }

    #[test]
    fn a_release_missing_from_the_snapshot_is_not_a_restore() {
        let mut snap = snapshot();
        snap.workloads = vec![held_through("web", "reaped", 0)];
        assert_eq!(
            reap_of(&decide(&snap, &Config::default()), "web"),
            Plan::Held
        );
    }

    // 002 US3: leaving scope is a restore (T137, FR-010)

    /// `web` in `app`, reaped through a Kustomization (reconcile was unset)
    /// and HelmRelease helm/app, which the reaper suspended for it alone.
    fn reaped_with_flux(state: &str) -> Snapshot {
        let mut snap = snapshot();
        add_release(&mut snap, &["web"], "true", true);
        let mut w = held_through("web", state, 0);
        w.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        w.flux.reconcile_annotation = Some(FLUX_RECONCILE_DISABLED.into());
        w.reaper_annotations
            .insert(PREVIOUS_RECONCILE.into(), "<none>".into());
        snap.workloads = vec![w];
        snap
    }

    fn excluding(ns: &str) -> Config {
        let mut config = Config::default();
        config.scope.exclude_namespaces = vec![ns.into()];
        config
    }

    fn opted_out_with_flux_released(p: &CyclePlan) {
        let (kind, actions) = restore_of(p, "web");
        assert_eq!(kind, RestoreKind::OptedOut);
        assert_eq!(
            actions,
            [
                Action::ReleaseHelmRelease {
                    helm_release: NsName::new("helm", "app"),
                    resume: true,
                    restore_reconcile: None,
                },
                Action::ScaleTo { replicas: 3 },
                Action::ClearReaperState {
                    restore_reconcile: Some(PreviousReconcile::Absent),
                    last_counted_job: None,
                },
            ]
        );
    }

    #[test]
    fn an_excluded_namespace_restores_what_the_reaper_holds_there() {
        let p = decide(&reaped_with_flux("reaped"), &excluding("app"));
        opted_out_with_flux_released(&p);
    }

    #[test]
    fn a_namespace_left_out_of_the_include_list_restores_too() {
        let mut config = Config::default();
        config.scope.include_namespaces = vec!["shop-*".into()];
        opted_out_with_flux_released(&decide(&reaped_with_flux("reaped"), &config));
    }

    #[test]
    fn an_excluded_reap_under_way_is_restored() {
        let p = decide(&reaped_with_flux("reaping"), &excluding("a*"));
        opted_out_with_flux_released(&p);
    }

    #[test]
    fn out_of_scope_without_a_reap_decides_nothing() {
        let mut snap = snapshot();
        for (name, state) in [
            ("fresh", None),
            ("seen", Some("observed")),
            ("due", Some("candidate")),
        ] {
            let mut d = add_looping(&mut snap, name, 60);
            d.state_label = state.map(str::to_owned);
            if state.is_none() {
                d.reaper_annotations.clear();
            }
            snap.workloads.push(d);
        }
        assert_eq!(snap.workloads.len(), 3);
        let p = decide(&snap, &excluding("app"));
        assert_eq!(p.decisions, Vec::<Decision>::new());
    }

    #[test]
    fn a_forged_record_in_the_reapers_own_namespace_decides_nothing() {
        let mut snap = snapshot();
        let mut w = deployment("k8s-reaper", "k8s-reaper");
        w.state_label = Some("reaped".into());
        w.spec_replicas = Some(0);
        w.reaper_annotations
            .insert(ORIGINAL_REPLICAS.into(), "1".into());
        snap.workloads = vec![w];
        assert_eq!(
            decide(&snap, &Config::default()).decisions,
            Vec::<Decision>::new()
        );
        // Not even when the scope lists it.
        let mut config = Config::default();
        config.scope.include_namespaces = vec!["k8s-*".into()];
        assert_eq!(decide(&snap, &config).decisions, Vec::<Decision>::new());
    }

    #[test]
    fn restores_ignore_the_reap_limits() {
        let mut snap = reaped_with_flux("reaped");
        snap.workloads[0].workload.namespace = "gone".into();
        let web = add_looping(&mut snap, "api", 60);
        snap.workloads.push(web);
        let mut config = excluding("gone");
        config.limits.max_reaps_per_cycle = 1;
        let p = decide(&snap, &config);
        // The restore does not use up this cycle's reap.
        assert!(matches!(reap_of(&p, "api"), Plan::Reap { .. }));
        assert_eq!(restore_of(&p, "web").0, RestoreKind::OptedOut);

        // Nor is it ever rate-limited or held back by the ceiling.
        config.limits.max_reaps_per_cycle = 0;
        config.limits.max_reaped = 0;
        let p = decide(&snap, &config);
        assert_eq!(skip_of(&reap_of(&p, "api")), Some(SkipReason::RateLimited));
        assert_eq!(restore_of(&p, "web").0, RestoreKind::OptedOut);
    }

    // US5: CronJobs (T081)

    /// A CronJob whose Jobs failed `failed` minutes ago, never observed.
    fn failing_cron(name: &str, failed: &[i64]) -> WorkloadView {
        let mut c = cron_job("app", name);
        cron_mut(&mut c).jobs = failed
            .iter()
            .enumerate()
            .map(|(i, m)| failed_job(&format!("{name}-{i}"), *m))
            .collect();
        c
    }

    /// A CronJob the reaper holds in `state`; `suspended` as given.
    fn held_cron(state: &str, suspended: bool) -> WorkloadView {
        let mut c = failing_cron("ingest", &[400, 300, 200]);
        c.state_label = Some(state.into());
        c.reaper_annotations
            .insert(FAILING_SINCE.into(), mins_ago(400).to_string());
        cron_mut(&mut c).suspended = suspended;
        c
    }

    fn cron_plan(w: WorkloadView) -> Plan {
        reap_of(&run(snapshot(), vec![w]), "ingest")
    }

    fn cron_reap() -> Vec<Action> {
        vec![
            Action::RecordReap {
                original_replicas: None,
            },
            Action::SuspendCronJob,
            Action::MarkReaped,
        ]
    }

    #[test]
    fn cron_candidate_at_exactly_the_duration_and_runs() {
        let p = run(snapshot(), vec![failing_cron("ingest", &[360, 300, 240])]);
        let d = only(&p);
        assert_eq!(
            d.plan,
            Plan::Reap {
                actions: cron_reap()
            }
        );
        assert_eq!(d.since, Some(mins_ago(360)));
        assert_eq!(
            d.evidence,
            Some(Evidence::JobFailures {
                failed_runs: 3,
                last_failed_job: "ingest-2".into(),
                last_failure_reason: Some("BackoffLimitExceeded".into()),
                streak_since: Some(mins_ago(360)),
                streak_met: true,
                // Spec 003: three runs do not fill the window.
                rate: Some(RateEvidence {
                    failed: 3,
                    window: 3,
                    met: false,
                }),
            })
        );
    }

    #[test]
    fn cron_below_either_threshold_is_observed() {
        assert_eq!(
            cron_plan(failing_cron("ingest", &[359, 300, 240])),
            Plan::Observe
        );
        // Two runs over three days.
        assert_eq!(
            cron_plan(failing_cron("ingest", &[3 * 24 * 60, 10])),
            Plan::Observe
        );
    }

    #[test]
    fn cron_overrides_raise_the_thresholds() {
        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.reaper_annotations
            .insert(MIN_FAILED_RUNS.into(), "4".into());
        assert_eq!(cron_plan(c), Plan::Observe);
        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.reaper_annotations
            .insert(FAILING_DURATION.into(), "7h".into());
        assert_eq!(cron_plan(c), Plan::Observe);
    }

    #[test]
    fn cron_suspended_by_someone_else_is_never_a_candidate() {
        let mut c = failing_cron("ingest", &[400, 300, 240]);
        cron_mut(&mut c).suspended = true;
        assert_eq!(
            run(snapshot(), vec![c.clone()]).decisions,
            Vec::<Decision>::new()
        );
        // Observed before: state is cleared.
        c.state_label = Some("candidate".into());
        c.reaper_annotations
            .insert(FAILING_SINCE.into(), mins_ago(400).to_string());
        assert_eq!(cron_plan(c), Plan::Forget);
    }

    #[test]
    fn cron_success_clears_state_at_once() {
        let mut c = failing_cron("ingest", &[400, 300]);
        cron_mut(&mut c).jobs.push(succeeded_job("ingest-9", 1));
        c.state_label = Some("observed".into());
        c.reaper_annotations
            .insert(FAILING_SINCE.into(), mins_ago(400).to_string());
        assert_eq!(cron_plan(c), Plan::Recover);
    }

    #[test]
    fn cron_skip_reasons_that_apply_and_those_that_never_do() {
        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.has_controller_owner = true;
        assert_eq!(
            skip_of(&cron_plan(c)),
            Some(SkipReason::ManagedByController)
        );

        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.mode_label = Some("report".into());
        assert_eq!(skip_of(&cron_plan(c)), Some(SkipReason::ReportOnlyScope));

        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.mode_label = Some("off".into());
        assert_eq!(run(snapshot(), vec![c]).decisions, Vec::<Decision>::new());

        let mut snap = snapshot();
        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.available_replicas = 1;
        snap.keda_targets.insert(c.workload.clone());
        snap.workloads = vec![c];
        let p = decide(&snap, &Config::default());
        assert_eq!(
            reap_of(&p, "ingest"),
            Plan::Reap {
                actions: cron_reap()
            }
        );
    }

    #[test]
    fn cron_shares_the_limits_with_deployments() {
        let mut snap = snapshot();
        let web = add_looping(&mut snap, "web", 60);
        snap.workloads = vec![web, failing_cron("ingest", &[400, 300, 240])];
        let p = decide(&snap, &Config::default());
        // The CronJob has failed longer, so it goes first.
        assert!(matches!(reap_of(&p, "ingest"), Plan::Reap { .. }));
        assert_eq!(skip_of(&reap_of(&p, "web")), Some(SkipReason::RateLimited));

        snap.workloads = vec![
            held("old", "reaped", 0),
            failing_cron("ingest", &[400, 300, 240]),
        ];
        let mut config = Config::default();
        config.limits.max_reaped = 1;
        let p = decide(&snap, &config);
        assert_eq!(
            skip_of(&reap_of(&p, "ingest")),
            Some(SkipReason::ReapCeilingReached)
        );
    }

    #[test]
    fn cron_flux_is_held_as_for_deployments() {
        let mut snap = snapshot();
        let mut c = failing_cron("ingest", &[400, 300, 240]);
        c.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        c.flux.helm_release = Some(NsName::new("app", "ingest"));
        snap.helm_releases
            .insert(NsName::new("app", "ingest"), helm_release("app", "ingest"));
        snap.workloads = vec![c];
        let p = decide(&snap, &Config::default());
        assert_eq!(
            reap_of(&p, "ingest"),
            Plan::Reap {
                actions: vec![
                    Action::RecordReap {
                        original_replicas: None
                    },
                    Action::DisableKustomizeReconcile(
                        WorkloadRef::new(WorkloadKind::CronJob, "app", "ingest").object_ref()
                    ),
                    Action::HoldHelmRelease {
                        helm_release: NsName::new("app", "ingest"),
                        suspend: true,
                        disable_reconcile: false,
                    },
                    Action::SuspendCronJob,
                    Action::MarkReaped,
                ]
            }
        );
    }

    #[test]
    fn cron_interrupted_reap_resumes_from_what_is_visible() {
        assert_eq!(
            cron_plan(held_cron("reaping", false)),
            Plan::Reap {
                actions: vec![Action::SuspendCronJob, Action::MarkReaped]
            }
        );
        assert_eq!(
            cron_plan(held_cron("reaping", true)),
            Plan::Reap {
                actions: vec![Action::MarkReaped]
            }
        );
        assert_eq!(cron_plan(held_cron("reaped", true)), Plan::Held);
    }

    #[test]
    fn cron_restore_annotation_resumes_and_clears() {
        let mut c = held_cron("reaped", true);
        c.reaper_annotations.insert(RESTORE.into(), "true".into());
        let (kind, actions) = restore_of(&run(snapshot(), vec![c]), "ingest");
        assert_eq!(kind, RestoreKind::Requested);
        assert_eq!(
            actions,
            [
                Action::ResumeCronJob,
                Action::ClearReaperState {
                    restore_reconcile: None,
                    last_counted_job: Some("ingest-2".into()),
                }
            ]
        );
    }

    #[test]
    fn cron_manual_resume_is_a_restore() {
        let c = held_cron("reaped", false);
        assert!(wants_immediate_cycle(&c));
        assert!(!wants_immediate_cycle(&held_cron("reaped", true)));
        let (kind, actions) = restore_of(&run(snapshot(), vec![c]), "ingest");
        assert_eq!(kind, RestoreKind::ManualResume);
        assert_eq!(
            actions,
            [Action::ClearReaperState {
                restore_reconcile: None,
                last_counted_job: Some("ingest-2".into()),
            }]
        );
    }

    #[test]
    fn cron_disabled_stops_observation_but_not_restore() {
        let mut config = Config::default();
        config.cron_jobs.enabled = false;
        let mut snap = snapshot();
        snap.workloads = vec![failing_cron("ingest", &[400, 300, 240])];
        assert_eq!(decide(&snap, &config).decisions, Vec::<Decision>::new());

        let mut c = held_cron("reaped", true);
        c.reaper_annotations.insert(RESTORE.into(), "true".into());
        snap.workloads = vec![c];
        let p = decide(&snap, &config);
        assert_eq!(restore_of(&p, "ingest").0, RestoreKind::Requested);
    }

    // Spec 003, US1: the failure-rate rule (T221)

    fn history_of(d: &Decision) -> String {
        d.run_history
            .as_ref()
            .map(RunHistory::encode)
            .unwrap_or_default()
    }

    #[test]
    fn rate_only_candidate_below_the_streak_thresholds() {
        // The streak since the last success is 2 runs, 2 minutes old.
        let p = run(snapshot(), vec![flaky_cron("FFSFFFFSFF")]);
        let d = only(&p);
        assert_eq!(
            d.plan,
            Plan::Reap {
                actions: cron_reap()
            }
        );
        assert_eq!(d.since, Some(mins_ago(7 * 60)));
        assert_eq!(
            d.evidence,
            Some(Evidence::JobFailures {
                failed_runs: 2,
                last_failed_job: "ingest-9".into(),
                last_failure_reason: Some("BackoffLimitExceeded".into()),
                streak_since: Some(mins_ago(2)),
                streak_met: false,
                rate: Some(RateEvidence {
                    failed: 8,
                    window: 10,
                    met: true,
                }),
            })
        );
        // Nothing new was recorded.
        let c = flaky_cron("FFSFFFFSFF");
        assert_eq!(&history_of(d), &c.reaper_annotations[RUN_HISTORY]);
    }

    #[test]
    fn rate_candidate_is_held_by_flux_as_for_the_streak() {
        let mut snap = snapshot();
        let mut c = flaky_cron("FFSFFFFSFF");
        c.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        snap.workloads = vec![c];
        let p = decide(&snap, &Config::default());
        let Plan::Reap { actions } = reap_of(&p, "ingest") else {
            panic!("not a reap");
        };
        assert!(actions.contains(&Action::SuspendCronJob));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::DisableKustomizeReconcile(_)))
        );
    }

    #[test]
    fn rate_rule_disabled_leaves_the_streak_alone() {
        let mut config = Config::default();
        config.cron_jobs.failure_rate.enabled = false;
        let mut snap = snapshot();
        snap.workloads = vec![flaky_cron("FFSFFFFSFF")];
        let p = decide(&snap, &config);
        let d = only(&p);
        assert_eq!(d.plan, Plan::Observe);
        assert_eq!(d.since, Some(mins_ago(2)));
        assert_eq!(d.run_history, None);
    }

    #[test]
    fn newest_run_succeeded_while_the_window_holds_failures_is_observed() {
        let p = run(snapshot(), vec![flaky_cron("FFFFFFFFFS")]);
        let d = only(&p);
        assert_eq!(d.plan, Plan::Observe);
        assert_eq!(d.since, Some(mins_ago(7 * 60)));
        assert_eq!(
            d.evidence,
            Some(Evidence::JobFailures {
                failed_runs: 0,
                last_failed_job: "ingest-8".into(),
                last_failure_reason: Some("BackoffLimitExceeded".into()),
                streak_since: None,
                streak_met: false,
                rate: Some(RateEvidence {
                    failed: 9,
                    window: 10,
                    met: false,
                }),
            })
        );
    }

    #[test]
    fn window_cleared_with_a_passing_streak_recovers() {
        let mut config = Config::default();
        config.cron_jobs.failure_rate.window_runs = 3;
        let mut c = flaky_cron("FSS");
        cron_mut(&mut c).jobs.push(succeeded_job("ingest-3", 0));
        cron_mut(&mut c).last_successful_time = Some(NOW);
        let mut snap = snapshot();
        snap.workloads = vec![c];
        let p = decide(&snap, &config);
        assert_eq!(only(&p).plan, Plan::Recover);
    }

    #[test]
    fn rate_candidates_are_ordered_by_history_start() {
        let mut snap = snapshot();
        let web = add_looping(&mut snap, "web", 60);
        snap.workloads = vec![web, flaky_cron("FFSFFFFSFF")];
        let p = decide(&snap, &Config::default());
        // The history began seven hours ago, before the crash loop.
        assert!(matches!(reap_of(&p, "ingest"), Plan::Reap { .. }));
        assert_eq!(skip_of(&reap_of(&p, "web")), Some(SkipReason::RateLimited));
    }

    // Spec 003 US3: per-CronJob overrides and turning the rule off.

    /// Thresholds for `app/ingest` with these annotations, against the
    /// default cluster window of 10 and threshold of 80.
    fn rate_overrides(pairs: &[(&str, &str)]) -> (Thresholds, Vec<String>) {
        let mut w = cron_job("app", "ingest");
        for (k, v) in pairs {
            w.reaper_annotations.insert((*k).into(), (*v).into());
        }
        let mut invalid = BTreeSet::new();
        let t = Thresholds::for_workload(&Config::default(), &w, &mut invalid);
        (t, invalid.into_iter().map(|i| i.key).collect())
    }

    #[test]
    fn failure_window_override_only_raises_it() {
        let window = |v| {
            let (t, invalid) = rate_overrides(&[(FAILURE_WINDOW, v)]);
            (t.window_runs, invalid)
        };
        assert_eq!(window("15"), (15, vec![]));
        assert_eq!(window("5"), (10, vec![]));
        for bad in ["2", "101", "x"] {
            assert_eq!(window(bad), (10, vec![FAILURE_WINDOW.to_owned()]), "{bad}");
        }
    }

    #[test]
    fn failure_threshold_override_only_raises_it() {
        let threshold = |v| {
            let (t, invalid) = rate_overrides(&[(FAILURE_THRESHOLD, v)]);
            (t.threshold, invalid)
        };
        assert_eq!(threshold("90"), (90, vec![]));
        assert_eq!(threshold("90%"), (90, vec![]));
        assert_eq!(threshold("60"), (80, vec![]));
        for bad in ["50", "abc"] {
            assert_eq!(
                threshold(bad),
                (80, vec![FAILURE_THRESHOLD.to_owned()]),
                "{bad}"
            );
        }
    }

    #[test]
    fn failure_rate_off_is_the_only_value_it_takes() {
        let enabled = |v| {
            let (t, invalid) = rate_overrides(&[(FAILURE_RATE, v)]);
            (t.failure_rate_enabled, invalid)
        };
        assert_eq!(enabled("off"), (false, vec![]));
        for bad in ["on", "yes"] {
            assert_eq!(enabled(bad), (true, vec![FAILURE_RATE.to_owned()]), "{bad}");
        }
        assert!(rate_overrides(&[]).0.failure_rate_enabled);
    }

    #[test]
    fn rate_rule_off_by_annotation_leaves_the_streak_alone() {
        let mut c = flaky_cron("FFSFFFFSFF");
        c.reaper_annotations
            .insert(FAILURE_RATE.into(), "off".into());
        let p = run(snapshot(), vec![c]);
        let d = only(&p);
        assert_eq!(d.plan, Plan::Observe);
        assert_eq!(d.since, Some(mins_ago(2)));
        assert_eq!(d.run_history, None);
    }

    #[test]
    fn rate_rule_off_without_a_streak_forgets_the_history_quietly() {
        let mut c = flaky_cron("FFFFFFFFFS");
        c.reaper_annotations
            .insert(FAILURE_RATE.into(), "off".into());
        let p = run(snapshot(), vec![c]);
        let d = only(&p);
        assert_eq!(d.plan, Plan::Forget);
        assert_eq!(d.run_history, None);
    }

    #[test]
    fn rate_rule_turned_back_on_starts_from_the_current_streak() {
        let mut c = flaky_cron("FFSFF");
        c.reaper_annotations.remove(RUN_HISTORY);
        let p = run(snapshot(), vec![c]);
        let h = only(&p).run_history.clone().expect("a new history");
        assert_eq!(h.since, mins_ago(2));
        assert!(h.runs.iter().all(|r| !r.ok), "{h:?}");
        assert_eq!(h.runs.len(), 2, "{h:?}");
    }
}
