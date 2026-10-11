//! Carries out a cycle's plan through the ports (FR-006, FR-012).
//!
//! Every decision first publishes the reaper's own record: state label,
//! crash-loop timestamps and the plan annotation. Those writes are identical
//! in report and full mode, which is what makes report mode an honest dry
//! run (SC-004). The mode gate then stops before any [`Action`] that
//! [`mutates_cluster`](Action::mutates_cluster).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::config::Mode;
use crate::model::{Snapshot, WorkloadRef, WorkloadView};
use crate::notify::Notification;
use crate::plan::{Action, CyclePlan, Decision, Evidence, Plan, SkipReason, Transition};
use crate::ports::{
    ClusterWriter, EventRecord, EventSink, EventType, Telemetry, WorkloadRow, WriteError,
};
use crate::record::{MetadataPatch, State, WorkloadRecord};
use crate::time::Timestamp;

/// Events are capped at 1 KB (contracts/events.md); the sink truncates, this
/// only keeps notes from growing without bound.
const MAX_ACTIONS_IN_NOTE: usize = 8;

/// A write that failed this cycle. The workload's record is unchanged, so the
/// same transition is attempted again next cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub workload: WorkloadRef,
    pub error: WriteError,
}

/// What the executor did, for the caller to log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub failures: Vec<Failure>,
    /// Events that could not be published; they are not retried.
    pub event_failures: Vec<(WorkloadRef, &'static str, WriteError)>,
    /// Every transition this cycle, for notification channels (US6). Sent
    /// whether or not its Event was published.
    pub notifications: Vec<Notification>,
}

/// Runs one cycle's plan. With the global mode `Off` nothing is written.
pub async fn execute<W, E, T>(
    snap: &Snapshot,
    plan: &CyclePlan,
    mode: Mode,
    writer: &W,
    events: &E,
    telemetry: &T,
) -> Outcome
where
    W: ClusterWriter,
    E: EventSink,
    T: Telemetry,
{
    let mut outcome = Outcome::default();
    if mode == Mode::Off {
        return outcome;
    }
    let views: BTreeMap<&WorkloadRef, &WorkloadView> =
        snap.workloads.iter().map(|w| (&w.workload, w)).collect();
    let mut states: BTreeMap<WorkloadRef, Option<State>> = BTreeMap::new();

    for decision in &plan.decisions {
        let Some(view) = views.get(&decision.workload) else {
            continue;
        };
        let (current, _) =
            WorkloadRecord::decode(&view.reaper_annotations, view.state_label.as_deref());
        let desired = desired_record(&current, decision);
        let (desired_annotations, desired_label) = desired.encode();
        let patch = MetadataPatch::between(
            &view.reaper_annotations,
            view.state_label.as_deref(),
            &desired_annotations,
            desired_label.as_deref(),
        );
        if !patch.is_empty() {
            let target = decision.workload.object_ref();
            if let Err(error) = writer
                .patch_metadata(&target, &view.resource_version, &patch)
                .await
            {
                outcome.failures.push(Failure {
                    workload: decision.workload.clone(),
                    error,
                });
                continue;
            }
        }
        states.insert(decision.workload.clone(), desired.state);

        if let Some(transition) = transition(&current, decision, mode) {
            let event = EventRecord {
                regarding: decision.workload.object_ref(),
                reason: transition.reason(),
                event_type: if transition.is_warning() {
                    EventType::Warning
                } else {
                    EventType::Normal
                },
                action: transition.action(),
                note: note(decision, snap.now),
            };
            if let Err(e) = events.publish(&event).await {
                outcome
                    .event_failures
                    .push((decision.workload.clone(), event.reason, e));
            }
            outcome.notifications.push(Notification {
                reason: event.reason,
                event_type: event.event_type,
                regarding: event.regarding,
                state: desired.state,
                mode,
                note: event.note,
                time: snap.now,
            });
        }

        // Mode gate (FR-012): report mode stops here. Executing the reap
        // steps in full mode lands with US2 (T044); until then full mode
        // stops here too.
    }

    telemetry.workloads(&rows(snap, plan, &states));
    outcome
}

/// The record a decision leaves on its workload.
fn desired_record(current: &WorkloadRecord, d: &Decision) -> WorkloadRecord {
    match &d.plan {
        Plan::Observe | Plan::Reap { .. } | Plan::Skip { .. } => WorkloadRecord {
            state: d.plan.resulting_state(current.state),
            crashloop_since: d.since,
            healthy_since: d.healthy_since,
            plan: d.plan.published_json(),
            ..current.clone()
        },
        Plan::Recover | Plan::Forget => WorkloadRecord::default(),
        // Held, restores and anything not yet decided by US1 keep the record.
        _ => current.clone(),
    }
}

/// The Event a decision is worth, if it changes something (contracts/events.md).
fn transition(current: &WorkloadRecord, d: &Decision, mode: Mode) -> Option<Transition> {
    match &d.plan {
        Plan::Observe => current
            .state
            .is_none()
            .then_some(Transition::CrashLoopObserved),
        Plan::Reap { .. } => {
            // In full mode the `Reaped` event replaces this (T044).
            let entering = current.state != Some(State::Candidate);
            (entering && mode == Mode::Report).then_some(Transition::ReapPlanned)
        }
        Plan::Skip { reason, .. } => {
            let previous = current.plan.as_deref().and_then(published_skip);
            (previous != Some(*reason)).then_some(Transition::ReapSkipped(*reason))
        }
        Plan::Recover => Some(Transition::CrashLoopRecovered),
        _ => None,
    }
}

/// The skip reason in a published plan annotation, if it was a skip.
fn published_skip(json: &str) -> Option<SkipReason> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    serde_json::from_value(value.get("skip")?.clone()).ok()
}

/// Plain English, naming the container, restarts and crash-loop start.
fn note(d: &Decision, now: Timestamp) -> String {
    let mut s = String::new();
    if let Some(Evidence::Container {
        pod,
        container,
        restarts,
        last_exit,
    }) = &d.evidence
    {
        let _ = write!(
            s,
            "container {container} in pod {pod} restarted {restarts} times"
        );
        if let Some(exit) = last_exit {
            let _ = write!(s, ", last exit {} ({})", exit.exit_code, exit.reason);
        }
    }
    if let Some(since) = d.since {
        if !s.is_empty() {
            s.push_str("; ");
        }
        let minutes = now.seconds_since(since) / 60;
        let _ = write!(s, "crash-looping since {since} ({minutes}m)");
    }
    let (prefix, actions) = match &d.plan {
        Plan::Reap { actions } => ("full mode would", actions.as_slice()),
        Plan::Skip { reason, would } => {
            let _ = write!(s, "; reason={}", reason.as_str());
            ("would", would.as_slice())
        }
        _ => return s,
    };
    if !actions.is_empty() {
        let steps: Vec<String> = actions
            .iter()
            .take(MAX_ACTIONS_IN_NOTE)
            .map(describe)
            .collect();
        let _ = write!(s, "; {prefix}: {}", steps.join(", "));
    }
    s
}

fn describe(action: &Action) -> String {
    match action {
        Action::RecordReap {
            original_replicas: Some(n),
        } => format!("record {n} original replicas"),
        Action::RecordReap {
            original_replicas: None,
        } => "record the reap".into(),
        Action::DisableKustomizeReconcile(_) => "disable Kustomization reconcile".into(),
        Action::HoldHelmRelease {
            helm_release,
            suspend,
            ..
        } => {
            if *suspend {
                format!("suspend HelmRelease {helm_release}")
            } else {
                format!("hold HelmRelease {helm_release}")
            }
        }
        Action::ScaleTo { replicas } => format!("scale to {replicas}"),
        Action::SuspendCronJob => "suspend the CronJob".into(),
        Action::MarkReaped => "mark reaped".into(),
        Action::ReleaseHelmRelease { helm_release, .. } => {
            format!("release HelmRelease {helm_release}")
        }
        Action::ResumeCronJob => "resume the CronJob".into(),
        Action::ClearReaperState { .. } => "clear reaper state".into(),
    }
}

/// One row per workload left in a non-`none` state, whether or not it was
/// decided this cycle.
fn rows(
    snap: &Snapshot,
    plan: &CyclePlan,
    written: &BTreeMap<WorkloadRef, Option<State>>,
) -> Vec<WorkloadRow> {
    let decisions: BTreeMap<&WorkloadRef, &Decision> =
        plan.decisions.iter().map(|d| (&d.workload, d)).collect();
    snap.workloads
        .iter()
        .filter_map(|w| {
            let (record, _) =
                WorkloadRecord::decode(&w.reaper_annotations, w.state_label.as_deref());
            let decision = decisions.get(&w.workload);
            let state = written.get(&w.workload).copied().unwrap_or(record.state)?;
            let since = match decision {
                Some(d) if written.contains_key(&w.workload) => d.since,
                _ => record.crashloop_since,
            };
            let skipped = match decision.map(|d| &d.plan) {
                Some(Plan::Skip { reason, .. }) if written.contains_key(&w.workload) => {
                    Some(*reason)
                }
                _ => None,
            };
            Some(WorkloadRow {
                workload: w.workload.clone(),
                state,
                skipped,
                crashloop_seconds: since.map(|t| snap.now.seconds_since(t)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::decide::decide;
    use crate::record::{CRASHLOOP_SINCE, HEALTHY_SINCE, PLAN, STATE_LABEL};
    use crate::testing::{
        Call, FakeEvents, FakeTelemetry, FakeWriter, add_pod, block_on, crashlooping_container,
        deployment, healthy_container, mins_ago, snapshot,
    };

    struct Run {
        writer: FakeWriter,
        events: FakeEvents,
        telemetry: FakeTelemetry,
        outcome: Outcome,
    }

    fn run(snap: &Snapshot, mode: Mode) -> Run {
        let plan = decide(snap, &Config::default());
        let writer = FakeWriter::new();
        let events = FakeEvents::default();
        let telemetry = FakeTelemetry::default();
        let outcome = block_on(execute(snap, &plan, mode, &writer, &events, &telemetry));
        Run {
            writer,
            events,
            telemetry,
            outcome,
        }
    }

    /// A crash-looping Deployment with the given record.
    fn looping(state: Option<&str>, since_mins: Option<i64>, restarts: u32) -> Snapshot {
        let mut snap = snapshot();
        let mut d = deployment("app", "web");
        d.state_label = state.map(str::to_owned);
        if let Some(m) = since_mins {
            d.reaper_annotations
                .insert(CRASHLOOP_SINCE.into(), mins_ago(m).to_string());
        }
        add_pod(
            &mut snap,
            &d.workload,
            "web-1",
            vec![crashlooping_container("api", restarts)],
        );
        snap.workloads.push(d);
        snap
    }

    fn patch(call: &Call) -> &MetadataPatch {
        match call {
            Call::PatchMetadata { patch, .. } => patch,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn first_sighting_records_observed_and_emits_crash_loop_observed() {
        let r = run(&looping(None, None, 3), Mode::Report);
        let calls = r.writer.calls();
        assert_eq!(calls.len(), 1);
        let Call::PatchMetadata {
            resource_version, ..
        } = &calls[0]
        else {
            panic!()
        };
        assert_eq!(resource_version, "1");
        let p = patch(&calls[0]);
        assert_eq!(p.labels[STATE_LABEL].as_deref(), Some("observed"));
        assert_eq!(
            p.annotations[CRASHLOOP_SINCE].as_deref(),
            Some(snapshot().now.to_string().as_str())
        );
        assert_eq!(r.events.reasons(), ["CrashLoopObserved"]);
        let note = &r.events.events.lock().unwrap()[0].note;
        assert!(
            note.contains("container api in pod web-1 restarted 3 times"),
            "{note}"
        );
        let rows = r.telemetry.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, State::Observed);
    }

    #[test]
    fn unchanged_observation_writes_nothing_and_emits_nothing() {
        let r = run(&looping(Some("observed"), Some(5), 3), Mode::Report);
        assert_eq!(r.writer.calls(), Vec::<Call>::new());
        assert_eq!(r.events.reasons(), Vec::<&str>::new());
        assert_eq!(r.outcome.notifications, Vec::new());
        assert_eq!(
            r.telemetry.rows.lock().unwrap()[0].crashloop_seconds,
            Some(300)
        );
    }

    #[test]
    fn candidate_publishes_the_plan_and_reap_planned_once() {
        let r = run(&looping(Some("observed"), Some(60), 12), Mode::Report);
        let calls = r.writer.calls();
        let p = patch(&calls[0]);
        assert_eq!(p.labels[STATE_LABEL].as_deref(), Some("candidate"));
        assert!(p.annotations[PLAN].as_deref().unwrap().contains("scaleTo"));
        assert!(calls.iter().all(Call::only_reaper_metadata));
        assert_eq!(r.events.reasons(), ["ReapPlanned"]);
        let note = &r.events.events.lock().unwrap()[0].note;
        assert!(
            note.contains("full mode would: record 2 original replicas"),
            "{note}"
        );
        // The same transition goes to notifications, with the Event's note.
        let [n] = r.outcome.notifications.as_slice() else {
            panic!("{:?}", r.outcome.notifications)
        };
        assert_eq!(
            (n.reason, n.state, n.mode, &n.note),
            ("ReapPlanned", Some(State::Candidate), Mode::Report, note)
        );

        // Next cycle, already a candidate with the same plan: silent.
        let mut snap = looping(Some("candidate"), Some(61), 12);
        let plan_json = p.annotations[PLAN].clone().unwrap();
        snap.workloads[0]
            .reaper_annotations
            .insert(PLAN.into(), plan_json);
        let r = run(&snap, Mode::Report);
        assert_eq!(r.writer.calls(), Vec::<Call>::new());
        assert_eq!(r.events.reasons(), Vec::<&str>::new());
    }

    #[test]
    fn skip_reason_events_fire_on_entry_and_on_change_only() {
        let mut snap = looping(Some("observed"), Some(60), 12);
        snap.workloads[0].available_replicas = 1;
        let r = run(&snap, Mode::Report);
        assert_eq!(r.events.reasons(), ["ReapSkipped"]);
        let published = patch(&r.writer.calls()[0]).annotations[PLAN]
            .clone()
            .unwrap();
        assert!(
            r.events.events.lock().unwrap()[0]
                .note
                .contains("reason=PartiallyAvailable")
        );
        assert_eq!(
            r.telemetry.rows.lock().unwrap()[0].skipped,
            Some(SkipReason::PartiallyAvailable)
        );

        snap.workloads[0].state_label = Some("candidate".into());
        snap.workloads[0]
            .reaper_annotations
            .insert(PLAN.into(), published);
        assert_eq!(
            run(&snap, Mode::Report).events.reasons(),
            Vec::<&str>::new()
        );

        snap.workloads[0].has_controller_owner = true;
        assert_eq!(run(&snap, Mode::Report).events.reasons(), ["ReapSkipped"]);
    }

    #[test]
    fn recovery_clears_every_owned_key() {
        let mut snap = looping(Some("candidate"), Some(60), 12);
        snap.workloads[0]
            .reaper_annotations
            .insert(HEALTHY_SINCE.into(), mins_ago(11).to_string());
        snap.workloads[0]
            .reaper_annotations
            .insert(PLAN.into(), "{}".into());
        snap.pods[0].containers = vec![healthy_container("api")];
        let r = run(&snap, Mode::Report);
        let calls = r.writer.calls();
        let p = patch(&calls[0]);
        assert_eq!(p.labels[STATE_LABEL], None);
        assert!(p.annotations.values().all(Option::is_none));
        assert_eq!(p.annotations.len(), 3);
        assert_eq!(r.events.reasons(), ["CrashLoopRecovered"]);
        assert_eq!(*r.telemetry.rows.lock().unwrap(), Vec::new());
    }

    #[test]
    fn failed_write_skips_the_event_and_reports_the_failure() {
        let snap = looping(None, None, 3);
        let plan = decide(&snap, &Config::default());
        let writer = FakeWriter::new();
        writer.fail_when(|_| Some(WriteError::Conflict));
        let events = FakeEvents::default();
        let telemetry = FakeTelemetry::default();
        let outcome = block_on(execute(
            &snap,
            &plan,
            Mode::Report,
            &writer,
            &events,
            &telemetry,
        ));
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].error, WriteError::Conflict);
        assert_eq!(events.reasons(), Vec::<&str>::new());
        assert_eq!(*telemetry.rows.lock().unwrap(), Vec::new());
    }

    #[test]
    fn full_mode_never_mutates_before_us2() {
        let r = run(&looping(Some("observed"), Some(60), 12), Mode::Full);
        assert!(r.writer.calls().iter().all(Call::only_reaper_metadata));
        assert_eq!(r.outcome, Outcome::default());
    }

    #[test]
    fn mode_off_writes_nothing() {
        let r = run(&looping(None, None, 30), Mode::Off);
        assert_eq!(r.writer.calls(), Vec::<Call>::new());
        assert_eq!(r.events.reasons(), Vec::<&str>::new());
    }
}
