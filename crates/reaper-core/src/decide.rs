//! `decide()`: snapshot + config → one plan per workload (FR-003 to FR-011,
//! FR-017, FR-027, FR-034).
//!
//! The global mode is deliberately not an input: report and full mode must
//! publish identical plans (FR-006, SC-004). Only a `mode=report` label or
//! annotation turns a plan into a `ReportOnlyScope` skip.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::config::{
    CRASH_LOOP_DURATION_RANGE, Config, MIN_RESTARTS_RANGE, Mode, parse_mode_override,
};
use crate::detect::{WorkloadPods, group_pods};
use crate::glob::any_matches;
use crate::model::{ObjectKind, ObjectRef, Snapshot, WorkloadKind, WorkloadView};
use crate::plan::{Action, CyclePlan, Decision, Evidence, InvalidSetting, Plan, SkipReason};
use crate::record::{CRASHLOOP_DURATION, MIN_RESTARTS, MODE_KEY, State, WorkloadRecord};
use crate::time::parse_duration;

/// Decides every in-scope workload. Decisions come back longest crash-looping
/// first, then by namespace and name (FR-017).
#[must_use]
pub fn decide(snap: &Snapshot, config: &Config) -> CyclePlan {
    let in_scope = |ns: &str| namespace_in_scope(snap, config, ns);
    let (pods, unsupported) = group_pods(snap, in_scope);
    let mut invalid = BTreeSet::new();

    let mut decisions: Vec<Decision> = snap
        .workloads
        .iter()
        .filter(|w| in_scope(&w.workload.namespace))
        // CronJobs are decided from Jobs, not pods (US5).
        .filter(|w| w.workload.kind != WorkloadKind::CronJob)
        .map(|w| {
            let mode = label_mode(snap, w, &mut invalid);
            let thresholds = Thresholds::for_workload(config, w, &mut invalid);
            decide_workload(snap, config, w, pods.get(&w.workload), mode, &thresholds)
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

    CyclePlan {
        decisions,
        unsupported,
        invalid_settings: invalid.into_iter().collect(),
    }
}

/// FR-011: never the reaper's own namespace, never an excluded one, and only
/// included ones when an include list is set.
fn namespace_in_scope(snap: &Snapshot, config: &Config, ns: &str) -> bool {
    ns != snap.reaper_namespace
        && !any_matches(&config.scope.exclude_namespaces, ns)
        && (config.scope.include_namespaces.is_empty()
            || any_matches(&config.scope.include_namespaces, ns))
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

        let (min_secs, max_secs) = CRASH_LOOP_DURATION_RANGE;
        let duration = get(CRASHLOOP_DURATION).and_then(|v| match parse_duration(v) {
            Ok(d) if (min_secs..=max_secs).contains(&d.as_secs()) => Some(d),
            _ => {
                bad(CRASHLOOP_DURATION, v);
                None
            }
        });
        let (min_n, max_n) = MIN_RESTARTS_RANGE;
        let restarts = get(MIN_RESTARTS).and_then(|v| match v.parse::<u32>() {
            Ok(n) if (min_n..=max_n).contains(&n) => Some(n),
            _ => {
                bad(MIN_RESTARTS, v);
                None
            }
        });

        let global = &config.detection;
        Thresholds {
            crash_loop_duration: duration.map_or(global.crash_loop_duration, |d| {
                d.max(global.crash_loop_duration)
            }),
            min_restarts: restarts.map_or(global.min_restarts, |n| n.max(global.min_restarts)),
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
    };
    let has_state = record.state.is_some() || record.crashloop_since.is_some();

    match record.state {
        // Reap execution and restore are US2/US3 (T043, T05x).
        Some(State::Reaped) => return decision(Plan::Held, record.crashloop_since, None, None),
        Some(State::Reaping) => return decision(Plan::Nothing, None, None, None),
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

fn candidate_plan(snap: &Snapshot, config: &Config, w: &WorkloadView, mode: Mode) -> Plan {
    let skip = |reason, would| Plan::Skip { reason, would };
    if w.workload.kind == WorkloadKind::DaemonSet {
        return skip(SkipReason::CannotScaleToZero, Vec::new());
    }
    let would = reap_actions(snap, w);
    let flux_disabled = (w.flux.kustomization.is_some() && !config.flux.kustomize)
        || (w.flux.helm_release.is_some() && !config.flux.helm_release);
    let reason = if w.has_controller_owner {
        Some(SkipReason::ManagedByController)
    } else if snap.keda_targets.contains(&w.workload) {
        Some(SkipReason::ManagedByAutoscaler)
    } else if mode < Mode::Full || flux_disabled {
        Some(SkipReason::ReportOnlyScope)
    } else if w.available_replicas > 0 {
        Some(SkipReason::PartiallyAvailable)
    } else {
        None
    };
    match reason {
        Some(reason) => skip(reason, would),
        None => Plan::Reap { actions: would },
    }
}

/// Every reap step for a workload nothing has been done to yet (research R6):
/// record, block Flux, scale, mark.
fn reap_actions(snap: &Snapshot, w: &WorkloadView) -> Vec<Action> {
    let mut actions = vec![Action::RecordReap {
        original_replicas: w.spec_replicas,
    }];
    if w.flux.kustomization.is_some() {
        actions.push(Action::DisableKustomizeReconcile(w.workload.object_ref()));
    }
    if let Some(hr) = &w.flux.helm_release {
        let view = snap.helm_releases.get(hr);
        actions.push(Action::HoldHelmRelease {
            helm_release: hr.clone(),
            suspend: !view.is_some_and(|v| v.suspended),
            disable_reconcile: view.is_some_and(|v| v.kustomization.is_some()),
        });
    }
    actions.push(Action::ScaleTo { replicas: 0 });
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
    use crate::record::{CRASHLOOP_SINCE, HEALTHY_SINCE};
    use crate::testing::{
        NOW, add_namespace, add_pod, crashlooping_container, deployment, healthy_container,
        helm_release, mins_ago, snapshot, workload,
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
        for ns in ["kube-system", "k8s-reaper", "other", "slam-prod"] {
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
        config.scope.include_namespaces = vec!["slam-*".into(), "k8s-*".into()];
        let p = decide(&snap, &config);
        let names: Vec<String> = p.decisions.iter().map(|d| d.workload.to_string()).collect();
        assert_eq!(names, ["Deployment/slam-prod/web"]);
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
}
