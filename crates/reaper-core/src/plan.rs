//! Decision output (data-model.md "Core model (output)").
//!
//! The plan JSON published in `reaper.boop.ninja/plan` carries no mode and no
//! clock reading beyond what came in with the snapshot, so report mode and full
//! mode publish identical bytes for identical inputs (SC-004).

use serde::{Deserialize, Serialize};

use crate::model::{NsName, ObjectRef, WorkloadRef};
use crate::record::{PreviousReconcile, State};
use crate::time::Timestamp;

/// One concrete step. Reap steps run in the order listed; each one is
/// detectable from object state, so an interrupted reap resumes (FR-016).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Action {
    /// Write state `reaping` with the original replicas (absent for a
    /// CronJob), reason and crashloop start.
    RecordReap {
        original_replicas: Option<u32>,
    },
    /// Set `kustomize.toolkit.fluxcd.io/reconcile: disabled` on the workload,
    /// recording the previous value.
    DisableKustomizeReconcile(ObjectRef),
    /// Add the workload to the HelmRelease's `held-for`, suspending it and
    /// disabling its kustomize reconcile when needed.
    HoldHelmRelease {
        helm_release: NsName,
        suspend: bool,
        disable_reconcile: bool,
    },
    ScaleTo {
        replicas: u32,
    },
    SuspendCronJob,
    MarkReaped,
    /// Remove the workload from `held-for`; resume and restore reconcile when
    /// it was the last one.
    ReleaseHelmRelease {
        helm_release: NsName,
        resume: bool,
        restore_reconcile: Option<PreviousReconcile>,
    },
    ResumeCronJob,
    /// Delete every owned annotation and the state label, putting the Flux
    /// reconcile annotation back when the reaper had changed it.
    ClearReaperState {
        restore_reconcile: Option<PreviousReconcile>,
    },
}

impl Action {
    /// True for steps that change something other than the reaper's own
    /// annotations and labels. Report mode stops before these (FR-012).
    #[must_use]
    pub fn mutates_cluster(&self) -> bool {
        !matches!(self, Action::RecordReap { .. } | Action::MarkReaped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SkipReason {
    OptedOut,
    ReportOnlyScope,
    CannotScaleToZero,
    UnsupportedOwner,
    ManagedByController,
    ManagedByAutoscaler,
    PartiallyAvailable,
    RateLimited,
    ReapCeilingReached,
}

impl SkipReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::OptedOut => "OptedOut",
            SkipReason::ReportOnlyScope => "ReportOnlyScope",
            SkipReason::CannotScaleToZero => "CannotScaleToZero",
            SkipReason::UnsupportedOwner => "UnsupportedOwner",
            SkipReason::ManagedByController => "ManagedByController",
            SkipReason::ManagedByAutoscaler => "ManagedByAutoscaler",
            SkipReason::PartiallyAvailable => "PartiallyAvailable",
            SkipReason::RateLimited => "RateLimited",
            SkipReason::ReapCeilingReached => "ReapCeilingReached",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreKind {
    Requested,
    ManualScaleUp,
    ManualResume,
    /// `mode=off` set on a reaped workload.
    OptedOut,
}

impl RestoreKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RestoreKind::Requested => "requested",
            RestoreKind::ManualScaleUp => "manual-scale-up",
            RestoreKind::ManualResume => "manual-resume",
            RestoreKind::OptedOut => "opted-out",
        }
    }
}

/// The decision for one workload in one cycle. Identical in both modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Out of scope or nothing to record.
    Nothing,
    /// Crash-looping (or failing) but below the thresholds.
    Observe,
    /// A candidate: the remaining reap steps. Report mode publishes them,
    /// full mode also runs them.
    Reap { actions: Vec<Action> },
    /// A candidate that will not be reaped; `would` is kept for reporting.
    Skip {
        reason: SkipReason,
        would: Vec<Action>,
    },
    /// Reaped and nothing has changed.
    Held,
    /// Healthy for the recovery window (or a CronJob run succeeded).
    Recover,
    Restore {
        kind: RestoreKind,
        actions: Vec<Action>,
    },
    /// `restore` on a workload the reaper holds no reap for: remove it.
    RestoreIgnored,
    /// Drop observation state without an Event (scaled to 0 by someone else,
    /// suspended by someone else, or opted out before any reap).
    Forget,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum PlanJson<'a> {
    #[serde(untagged)]
    Actions { actions: &'a [Action] },
    #[serde(untagged)]
    Skip {
        skip: SkipReason,
        would: &'a [Action],
    },
}

impl Plan {
    /// The `reaper.boop.ninja/plan` value, for plans that publish one.
    #[must_use]
    pub fn published_json(&self) -> Option<String> {
        let json = match self {
            Plan::Reap { actions } => PlanJson::Actions { actions },
            Plan::Skip { reason, would } => PlanJson::Skip {
                skip: *reason,
                would,
            },
            _ => return None,
        };
        Some(serde_json::to_string(&json).expect("plan JSON is plain data"))
    }

    /// The state label this plan leaves on the workload (`None` clears it).
    /// `current` is the state before the cycle.
    #[must_use]
    pub fn resulting_state(&self, current: Option<State>) -> Option<State> {
        match self {
            Plan::Nothing | Plan::RestoreIgnored => current,
            Plan::Observe => Some(State::Observed),
            Plan::Reap { .. } | Plan::Skip { .. } => match current {
                Some(State::Reaping) => Some(State::Reaping),
                _ => Some(State::Candidate),
            },
            Plan::Held => Some(State::Reaped),
            Plan::Recover | Plan::Restore { .. } | Plan::Forget => None,
        }
    }
}

/// The worst crash-looping container, or for a CronJob the failing streak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    Container {
        pod: String,
        container: String,
        restarts: u32,
        last_exit: Option<crate::model::Termination>,
    },
    JobFailures {
        failed_runs: u32,
        last_failed_job: String,
        last_failure_reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub workload: WorkloadRef,
    pub plan: Plan,
    /// When the crash loop or failing streak began, if it is ongoing.
    pub since: Option<Timestamp>,
    /// When a crash-looping workload was first seen healthy again; cleared
    /// when it crashes before the recovery window ends (FR-003).
    pub healthy_since: Option<Timestamp>,
    pub evidence: Option<Evidence>,
}

/// A pod the reaper cannot attribute to a supported workload (US1 scenario 5).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnsupportedPod {
    pub namespace: String,
    pub pod: String,
    /// `None` for a bare pod.
    pub owner_kind: Option<String>,
}

/// A person-set value under the prefix that could not be used (mode label,
/// threshold override).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InvalidSetting {
    pub object: ObjectRef,
    pub key: String,
    pub value: String,
}

/// Everything one cycle decided, in deterministic order (FR-017).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CyclePlan {
    pub decisions: Vec<Decision>,
    pub unsupported: Vec<UnsupportedPod>,
    pub invalid_settings: Vec<InvalidSetting>,
    /// HelmRelease holds to release because the workload is gone or no
    /// longer held.
    pub stale_holds: Vec<StaleHold>,
}

/// A workload in a HelmRelease's `held-for` that the reaper no longer holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StaleHold {
    pub helm_release: NsName,
    pub workload: WorkloadRef,
}

/// What an Event reports (contracts/events.md). Emitted only on change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    CrashLoopObserved,
    JobFailuresObserved,
    ReapPlanned,
    ReapSkipped(SkipReason),
    Reaped,
    ReapFailed,
    Restored(RestoreKind),
    RestoreFailed,
    RestoreIgnored,
    CrashLoopRecovered,
    JobFailuresRecovered,
    HelmReleaseHeld,
    HelmReleaseReleased,
}

impl Transition {
    /// The Event `reason`.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Transition::CrashLoopObserved => "CrashLoopObserved",
            Transition::JobFailuresObserved => "JobFailuresObserved",
            Transition::ReapPlanned => "ReapPlanned",
            Transition::ReapSkipped(_) => "ReapSkipped",
            Transition::Reaped => "Reaped",
            Transition::ReapFailed => "ReapFailed",
            Transition::Restored(_) => "Restored",
            Transition::RestoreFailed => "RestoreFailed",
            Transition::RestoreIgnored => "RestoreIgnored",
            Transition::CrashLoopRecovered => "CrashLoopRecovered",
            Transition::JobFailuresRecovered => "JobFailuresRecovered",
            Transition::HelmReleaseHeld => "HelmReleaseHeld",
            Transition::HelmReleaseReleased => "HelmReleaseReleased",
        }
    }

    #[must_use]
    pub fn is_warning(&self) -> bool {
        matches!(
            self,
            Transition::ReapPlanned
                | Transition::ReapSkipped(_)
                | Transition::Reaped
                | Transition::ReapFailed
                | Transition::RestoreFailed
                | Transition::HelmReleaseHeld
        )
    }

    /// The Event `action` field.
    #[must_use]
    pub fn action(&self) -> &'static str {
        match self {
            Transition::CrashLoopObserved | Transition::JobFailuresObserved => "Observe",
            Transition::ReapPlanned | Transition::ReapSkipped(_) => "Plan",
            Transition::Reaped | Transition::ReapFailed => "ScaleDown",
            Transition::Restored(_) | Transition::RestoreFailed | Transition::RestoreIgnored => {
                "Restore"
            }
            Transition::CrashLoopRecovered | Transition::JobFailuresRecovered => "Recover",
            Transition::HelmReleaseHeld => "Suspend",
            Transition::HelmReleaseReleased => "Resume",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorkloadKind;

    fn kustomize_reap() -> Plan {
        let w = WorkloadRef::new(WorkloadKind::Deployment, "app", "web");
        Plan::Reap {
            actions: vec![
                Action::RecordReap {
                    original_replicas: Some(2),
                },
                Action::DisableKustomizeReconcile(w.object_ref()),
                Action::HoldHelmRelease {
                    helm_release: NsName::new("helm", "web"),
                    suspend: true,
                    disable_reconcile: false,
                },
                Action::ScaleTo { replicas: 0 },
                Action::MarkReaped,
            ],
        }
    }

    #[test]
    fn reap_plan_json_is_stable() {
        insta::assert_snapshot!(kustomize_reap().published_json().unwrap(), @r#"{"actions":[{"recordReap":{"originalReplicas":2}},{"disableKustomizeReconcile":"Deployment/app/web"},{"holdHelmRelease":{"helmRelease":"helm/web","suspend":true,"disableReconcile":false}},{"scaleTo":{"replicas":0}},"markReaped"]}"#);
    }

    #[test]
    fn skip_plan_json_is_stable() {
        let p = Plan::Skip {
            reason: SkipReason::PartiallyAvailable,
            would: vec![
                Action::RecordReap {
                    original_replicas: Some(3),
                },
                Action::ScaleTo { replicas: 0 },
                Action::MarkReaped,
            ],
        };
        insta::assert_snapshot!(p.published_json().unwrap(), @r#"{"skip":"PartiallyAvailable","would":[{"recordReap":{"originalReplicas":3}},{"scaleTo":{"replicas":0}},"markReaped"]}"#);
        let empty = Plan::Skip {
            reason: SkipReason::CannotScaleToZero,
            would: vec![],
        };
        insta::assert_snapshot!(empty.published_json().unwrap(), @r#"{"skip":"CannotScaleToZero","would":[]}"#);
    }

    #[test]
    fn cronjob_and_restore_actions_serialise() {
        let p = Plan::Reap {
            actions: vec![
                Action::RecordReap {
                    original_replicas: None,
                },
                Action::SuspendCronJob,
                Action::MarkReaped,
            ],
        };
        insta::assert_snapshot!(p.published_json().unwrap(), @r#"{"actions":[{"recordReap":{"originalReplicas":null}},"suspendCronJob","markReaped"]}"#);
        let restore = vec![
            Action::ReleaseHelmRelease {
                helm_release: NsName::new("helm", "web"),
                resume: true,
                restore_reconcile: Some(PreviousReconcile::Absent),
            },
            Action::ScaleTo { replicas: 2 },
            Action::ClearReaperState {
                restore_reconcile: Some(PreviousReconcile::Value("enabled".into())),
            },
        ];
        insta::assert_snapshot!(serde_json::to_string(&restore).unwrap(), @r#"[{"releaseHelmRelease":{"helmRelease":"helm/web","resume":true,"restoreReconcile":"<none>"}},{"scaleTo":{"replicas":2}},{"clearReaperState":{"restoreReconcile":"enabled"}}]"#);
        let back: Vec<Action> =
            serde_json::from_str(&serde_json::to_string(&restore).unwrap()).unwrap();
        assert_eq!(back, restore);
    }

    #[test]
    fn only_skips_and_reaps_publish() {
        for p in [
            Plan::Nothing,
            Plan::Observe,
            Plan::Held,
            Plan::Recover,
            Plan::RestoreIgnored,
            Plan::Forget,
        ] {
            assert_eq!(p.published_json(), None, "{p:?}");
        }
    }

    #[test]
    fn report_mode_gate_classification() {
        assert!(
            !Action::RecordReap {
                original_replicas: Some(1)
            }
            .mutates_cluster()
        );
        assert!(!Action::MarkReaped.mutates_cluster());
        assert!(Action::ScaleTo { replicas: 0 }.mutates_cluster());
        assert!(Action::SuspendCronJob.mutates_cluster());
    }

    #[test]
    fn resulting_states() {
        assert_eq!(
            kustomize_reap().resulting_state(None),
            Some(State::Candidate)
        );
        assert_eq!(
            kustomize_reap().resulting_state(Some(State::Reaping)),
            Some(State::Reaping)
        );
        assert_eq!(
            Plan::Held.resulting_state(Some(State::Reaped)),
            Some(State::Reaped)
        );
        assert_eq!(
            Plan::RestoreIgnored.resulting_state(Some(State::Observed)),
            Some(State::Observed)
        );
        assert_eq!(Plan::Forget.resulting_state(Some(State::Candidate)), None);
    }

    #[test]
    fn transitions_match_events_contract() {
        assert_eq!(
            Transition::ReapSkipped(SkipReason::RateLimited).reason(),
            "ReapSkipped"
        );
        assert!(Transition::Reaped.is_warning());
        assert!(!Transition::Restored(RestoreKind::Requested).is_warning());
        assert_eq!(Transition::HelmReleaseHeld.action(), "Suspend");
    }
}
