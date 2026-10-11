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
use crate::hold;
use crate::model::{HelmReleaseView, NsName, ObjectRef, Snapshot, WorkloadRef, WorkloadView};
use crate::notify::Notification;
use crate::plan::{
    Action, CyclePlan, Decision, Evidence, Plan, RestoreKind, SkipReason, Transition,
};
use crate::ports::{
    ActionKind, ActionResult, ClusterWriter, EventRecord, EventSink, EventType, Telemetry,
    WorkloadRow, WriteError,
};
use crate::record::{
    FLUX_RECONCILE, FLUX_RECONCILE_DISABLED, HELMRELEASE, MetadataPatch, ORIGINAL_REPLICAS,
    PREVIOUS_RECONCILE, PreviousReconcile, REAPED_AT, REASON, STATE_LABEL, State, WorkloadRecord,
    is_owned_key,
};
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
    pub event_failures: Vec<(ObjectRef, &'static str, WriteError)>,
    /// Every transition this cycle, for notification channels (US6). Sent
    /// whether or not its Event was published.
    pub notifications: Vec<Notification>,
}

/// Runs one cycle's plan. With the global mode `Off` nothing is written.
/// Reap steps run only in full mode; restores and stale-hold releases run in
/// report mode too, since they only undo what the reaper did (FR-018).
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
    if mode == Mode::Off {
        return Outcome::default();
    }
    let mut run = Run {
        writer,
        events,
        telemetry,
        mode,
        now: snap.now,
        helm_releases: snap.helm_releases.clone(),
        outcome: Outcome::default(),
    };
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
        let mut live = Live::new(view);
        if let Err(error) = live.patch(writer, &patch).await {
            run.fail(&decision.workload, error);
            continue;
        }

        if let Some(transition) = transition(&current, decision, mode) {
            if transition == Transition::ReapPlanned {
                telemetry.action(ActionKind::Reap, ActionResult::Planned, mode);
            }
            let note = note(decision, snap.now);
            run.emit(
                decision.workload.object_ref(),
                &transition,
                note,
                desired.state,
            )
            .await;
        }

        match &decision.plan {
            // Mode gate (FR-012): report mode stops before the reap steps.
            Plan::Reap { actions } if mode == Mode::Full => {
                run.reap(&mut live, &current, decision, actions).await;
            }
            Plan::Restore { kind, actions } => {
                run.restore(&mut live, *kind, actions).await;
            }
            _ => {}
        }
        states.insert(decision.workload.clone(), live.state());
    }

    for stale in &plan.stale_holds {
        if let Err(error) = run.release(&stale.helm_release, &stale.workload).await {
            run.fail(&stale.workload, error);
        }
    }

    telemetry.workloads(&rows(snap, plan, &states));
    run.outcome
}

/// One workload as the steps see it: updated after every write, so each write
/// carries the `resourceVersion` the previous one returned.
struct Live {
    workload: WorkloadRef,
    resource_version: String,
    /// Only the annotations under `reaper.boop.ninja/`.
    annotations: BTreeMap<String, String>,
    state_label: Option<String>,
    reconcile: Option<String>,
    replicas: Option<u32>,
}

impl Live {
    fn new(view: &WorkloadView) -> Self {
        Live {
            workload: view.workload.clone(),
            resource_version: view.resource_version.clone(),
            annotations: view.reaper_annotations.clone(),
            state_label: view.state_label.clone(),
            reconcile: view.flux.reconcile_annotation.clone(),
            replicas: view.spec_replicas,
        }
    }

    fn state(&self) -> Option<State> {
        self.state_label.as_deref().and_then(|s| s.parse().ok())
    }

    async fn patch<W: ClusterWriter>(
        &mut self,
        writer: &W,
        patch: &MetadataPatch,
    ) -> Result<(), WriteError> {
        if patch.is_empty() {
            return Ok(());
        }
        self.resource_version = writer
            .patch_metadata(&self.workload.object_ref(), &self.resource_version, patch)
            .await?;
        for (k, v) in &patch.annotations {
            if k == FLUX_RECONCILE {
                self.reconcile.clone_from(v);
            } else if let Some(v) = v {
                self.annotations.insert(k.clone(), v.clone());
            } else {
                self.annotations.remove(k);
            }
        }
        if let Some(v) = patch.labels.get(STATE_LABEL) {
            self.state_label.clone_from(v);
        }
        Ok(())
    }

    /// Adds `reconcile: disabled` to `patch`, recording the previous value
    /// unless a previous attempt already did.
    fn disable_reconcile(&self, patch: &mut MetadataPatch) {
        if self.reconcile.as_deref() == Some(FLUX_RECONCILE_DISABLED) {
            return;
        }
        if !self.annotations.contains_key(PREVIOUS_RECONCILE) {
            patch.annotations.insert(
                PREVIOUS_RECONCILE.to_owned(),
                Some(PreviousReconcile::from_current(self.reconcile.as_deref()).encode()),
            );
        }
        patch.annotations.insert(
            FLUX_RECONCILE.to_owned(),
            Some(FLUX_RECONCILE_DISABLED.to_owned()),
        );
    }
}

struct Run<'a, W, E, T> {
    writer: &'a W,
    events: &'a E,
    telemetry: &'a T,
    mode: Mode,
    now: Timestamp,
    /// HelmReleases as the steps left them; several workloads can share one.
    helm_releases: BTreeMap<NsName, HelmReleaseView>,
    outcome: Outcome,
}

impl<W: ClusterWriter, E: EventSink, T: Telemetry> Run<'_, W, E, T> {
    fn fail(&mut self, workload: &WorkloadRef, error: WriteError) {
        self.outcome.failures.push(Failure {
            workload: workload.clone(),
            error,
        });
    }

    fn record(&self, kind: ActionKind, ok: bool) {
        let result = if ok {
            ActionResult::Success
        } else {
            ActionResult::Error
        };
        self.telemetry.action(kind, result, self.mode);
    }

    async fn emit(
        &mut self,
        regarding: ObjectRef,
        transition: &Transition,
        note: String,
        state: Option<State>,
    ) {
        let event = EventRecord {
            regarding,
            reason: transition.reason(),
            event_type: if transition.is_warning() {
                EventType::Warning
            } else {
                EventType::Normal
            },
            action: transition.action(),
            note,
        };
        if let Err(e) = self.events.publish(&event).await {
            self.outcome
                .event_failures
                .push((event.regarding.clone(), event.reason, e));
        }
        self.outcome.notifications.push(Notification {
            reason: event.reason,
            event_type: event.event_type,
            regarding: event.regarding,
            state,
            mode: self.mode,
            note: event.note,
            time: self.now,
        });
    }

    /// The reap steps of research R6, stopping at the first failure; the
    /// next cycle resumes from what is visible on the objects (FR-016).
    async fn reap(
        &mut self,
        live: &mut Live,
        record: &WorkloadRecord,
        d: &Decision,
        actions: &[Action],
    ) {
        let original = record.original_replicas.or(live.replicas);
        let mut held = Vec::new();
        let mut result = Ok(());
        for action in actions {
            result = self.reap_step(live, d, actions, action, &mut held).await;
            if let Some(kind) = step_kind(action) {
                self.record(kind, result.is_ok());
            }
            if result.is_err() {
                break;
            }
        }
        let workload = live.workload.object_ref();
        match result {
            Ok(()) => {
                self.record(ActionKind::Reap, true);
                let mut note = format!(
                    "scaled {} → 0",
                    original.map_or_else(|| "?".to_owned(), |n| n.to_string())
                );
                if let Some(reason) = live.annotations.get(REASON) {
                    let _ = write!(note, "; {reason}");
                }
                if live.reconcile.as_deref() == Some(FLUX_RECONCILE_DISABLED) {
                    note.push_str("; Kustomization reconcile disabled");
                }
                for hr in &held {
                    let _ = write!(note, "; HelmRelease {hr} suspended");
                }
                note.push_str("; restore with reaper.boop.ninja/restore=true");
                self.emit(workload, &Transition::Reaped, note, live.state())
                    .await;
            }
            Err(error) => {
                self.record(ActionKind::Reap, false);
                let note = format!("reap step failed: {error}; retried next cycle");
                self.emit(workload, &Transition::ReapFailed, note, live.state())
                    .await;
                self.fail(&live.workload, error);
            }
        }
    }

    async fn reap_step(
        &mut self,
        live: &mut Live,
        d: &Decision,
        actions: &[Action],
        action: &Action,
        held: &mut Vec<NsName>,
    ) -> Result<(), WriteError> {
        let writer = self.writer;
        match action {
            Action::RecordReap { original_replicas } => {
                let mut patch = MetadataPatch::default();
                if let Some(n) = original_replicas {
                    patch
                        .annotations
                        .insert(ORIGINAL_REPLICAS.to_owned(), Some(n.to_string()));
                }
                patch.annotations.insert(REASON.to_owned(), Some(reason(d)));
                patch.labels.insert(
                    STATE_LABEL.to_owned(),
                    Some(State::Reaping.as_str().to_owned()),
                );
                // Flux is blocked in the same write (R6 step 1).
                if actions
                    .iter()
                    .any(|a| matches!(a, Action::DisableKustomizeReconcile(_)))
                {
                    live.disable_reconcile(&mut patch);
                }
                live.patch(writer, &patch).await
            }
            Action::DisableKustomizeReconcile(_) => {
                let mut patch = MetadataPatch::default();
                live.disable_reconcile(&mut patch);
                live.patch(writer, &patch).await
            }
            Action::HoldHelmRelease { helm_release, .. } => {
                self.hold(live, helm_release, held).await
            }
            Action::ScaleTo { replicas } => {
                live.resource_version = writer
                    .scale(&live.workload, &live.resource_version, *replicas)
                    .await?;
                live.replicas = Some(*replicas);
                Ok(())
            }
            Action::SuspendCronJob => {
                live.resource_version = writer
                    .set_cronjob_suspend(&live.workload, &live.resource_version, true)
                    .await?;
                Ok(())
            }
            Action::MarkReaped => {
                let mut patch = MetadataPatch::default();
                patch
                    .annotations
                    .insert(REAPED_AT.to_owned(), Some(self.now.to_string()));
                patch.labels.insert(
                    STATE_LABEL.to_owned(),
                    Some(State::Reaped.as_str().to_owned()),
                );
                live.patch(writer, &patch).await
            }
            Action::ReleaseHelmRelease { .. }
            | Action::ResumeCronJob
            | Action::ClearReaperState { .. } => Ok(()),
        }
    }

    /// R5: one patch on the HelmRelease, then the workload records which one
    /// it holds.
    async fn hold(
        &mut self,
        live: &mut Live,
        hr: &NsName,
        held: &mut Vec<NsName>,
    ) -> Result<(), WriteError> {
        let view = self.helm_releases.get_mut(hr).ok_or(WriteError::NotFound)?;
        let change = hold::place(view, &live.workload);
        if !change.is_empty() {
            let rv = self
                .writer
                .patch_helm_release(hr, &view.resource_version, &change.patch)
                .await?;
            hold::apply(view, &change.patch, rv);
        }
        if change.patch.suspend == Some(true) {
            held.push(hr.clone());
        }
        if change.held {
            let note = format!(
                "suspended for {}: every resource in this release is frozen until the \
                 reaper releases it",
                live.workload
            );
            self.emit(
                ObjectRef::helm_release(hr),
                &Transition::HelmReleaseHeld,
                note,
                None,
            )
            .await;
        }
        let value = hr.to_string();
        if live.annotations.get(HELMRELEASE) != Some(&value) {
            let mut patch = MetadataPatch::default();
            patch
                .annotations
                .insert(HELMRELEASE.to_owned(), Some(value));
            live.patch(self.writer, &patch).await?;
        }
        Ok(())
    }

    /// Removes `workload` from a hold. A HelmRelease that is gone has nothing
    /// left to release.
    async fn release(&mut self, hr: &NsName, workload: &WorkloadRef) -> Result<(), WriteError> {
        let Some(view) = self.helm_releases.get_mut(hr) else {
            return Ok(());
        };
        let change = hold::release(view, workload);
        if !change.is_empty() {
            let written = self
                .writer
                .patch_helm_release(hr, &view.resource_version, &change.patch)
                .await;
            let result = if written.is_ok() {
                ActionResult::Success
            } else {
                ActionResult::Error
            };
            self.telemetry
                .action(ActionKind::HelmReleaseRelease, result, self.mode);
            match written {
                Ok(rv) => hold::apply(view, &change.patch, rv),
                Err(WriteError::NotFound) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        if change.released {
            let note = format!("released after {workload}; no workload holds it any more");
            self.emit(
                ObjectRef::helm_release(hr),
                &Transition::HelmReleaseReleased,
                note,
                None,
            )
            .await;
        }
        Ok(())
    }

    /// Research R6 restore: release, scale back, clear. A failure leaves the
    /// workload `reaped` (or `reaping`), so the next cycle tries again.
    async fn restore(&mut self, live: &mut Live, kind: RestoreKind, actions: &[Action]) {
        let mut result = Ok(());
        for action in actions {
            result = self.restore_step(live, action).await;
            if let Some(step) = step_kind(action) {
                self.record(step, result.is_ok());
            }
            if result.is_err() {
                break;
            }
        }
        let workload = live.workload.object_ref();
        match result {
            Ok(()) => {
                self.record(ActionKind::Restore, true);
                let mut note = kind.as_str().to_owned();
                if let Some(n) = live.replicas {
                    let _ = write!(note, "; {n} replicas");
                }
                if actions
                    .iter()
                    .any(|a| matches!(a, Action::ReleaseHelmRelease { .. }))
                {
                    note.push_str("; HelmRelease hold released");
                }
                if actions.iter().any(|a| {
                    matches!(
                        a,
                        Action::ClearReaperState {
                            restore_reconcile: Some(_)
                        }
                    )
                }) {
                    note.push_str("; Kustomization reconcile restored");
                }
                self.emit(workload, &Transition::Restored(kind), note, None)
                    .await;
            }
            Err(error) => {
                self.record(ActionKind::Restore, false);
                let note = format!("restore step failed: {error}; retried next cycle");
                self.emit(workload, &Transition::RestoreFailed, note, live.state())
                    .await;
                self.fail(&live.workload, error);
            }
        }
    }

    async fn restore_step(&mut self, live: &mut Live, action: &Action) -> Result<(), WriteError> {
        let writer = self.writer;
        match action {
            Action::ReleaseHelmRelease { helm_release, .. } => {
                let workload = live.workload.clone();
                self.release(helm_release, &workload).await
            }
            Action::ScaleTo { replicas } => {
                live.resource_version = writer
                    .scale(&live.workload, &live.resource_version, *replicas)
                    .await?;
                live.replicas = Some(*replicas);
                Ok(())
            }
            Action::ResumeCronJob => {
                live.resource_version = writer
                    .set_cronjob_suspend(&live.workload, &live.resource_version, false)
                    .await?;
                Ok(())
            }
            Action::ClearReaperState { restore_reconcile } => {
                let mut patch = MetadataPatch::default();
                for k in live.annotations.keys().filter(|k| is_owned_key(k)) {
                    patch.annotations.insert(k.clone(), None);
                }
                if let Some(previous) = restore_reconcile {
                    let restored = previous.restored_value();
                    if live.reconcile != restored {
                        patch
                            .annotations
                            .insert(FLUX_RECONCILE.to_owned(), restored);
                    }
                }
                if live.state_label.is_some() {
                    patch.labels.insert(STATE_LABEL.to_owned(), None);
                }
                live.patch(writer, &patch).await
            }
            Action::RecordReap { .. }
            | Action::DisableKustomizeReconcile(_)
            | Action::HoldHelmRelease { .. }
            | Action::SuspendCronJob
            | Action::MarkReaped => Ok(()),
        }
    }
}

/// The metric an action is counted under, for steps that touch the cluster.
fn step_kind(action: &Action) -> Option<ActionKind> {
    match action {
        Action::DisableKustomizeReconcile(_) => Some(ActionKind::KustomizeDisable),
        Action::HoldHelmRelease { .. } => Some(ActionKind::HelmReleaseHold),
        Action::ScaleTo { .. } => Some(ActionKind::Scale),
        Action::SuspendCronJob => Some(ActionKind::CronJobSuspend),
        Action::ResumeCronJob => Some(ActionKind::CronJobResume),
        Action::ClearReaperState {
            restore_reconcile: Some(_),
        } => Some(ActionKind::KustomizeRestore),
        // Releases are counted where the patch is written, so stale holds
        // count too.
        Action::ReleaseHelmRelease { .. }
        | Action::RecordReap { .. }
        | Action::MarkReaped
        | Action::ClearReaperState { .. } => None,
    }
}

/// The `reaper.boop.ninja/reason` text (contracts/annotations.md).
fn reason(d: &Decision) -> String {
    let mut s = String::new();
    if let Some(Evidence::Container {
        container,
        restarts,
        ..
    }) = &d.evidence
    {
        let _ = write!(
            s,
            "container \"{container}\" CrashLoopBackOff, {restarts} restarts"
        );
    }
    if let Some(since) = d.since {
        if !s.is_empty() {
            s.push_str(", ");
        }
        let _ = write!(s, "crash-looping since {since}");
    }
    if let Some(Evidence::Container {
        last_exit: Some(exit),
        ..
    }) = &d.evidence
    {
        let _ = write!(s, ", last exit {} ({})", exit.exit_code, exit.reason);
    }
    s
}

/// The record a decision leaves on its workload before any step runs.
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
        Plan::RestoreIgnored => WorkloadRecord {
            restore: false,
            ..current.clone()
        },
        // Held and restores keep the record; a restore clears it as its last
        // step.
        Plan::Nothing | Plan::Held | Plan::Restore { .. } => current.clone(),
    }
}

/// The Event a decision is worth before any step runs, if it changes
/// something (contracts/events.md). Step outcomes have their own Events.
fn transition(current: &WorkloadRecord, d: &Decision, mode: Mode) -> Option<Transition> {
    match &d.plan {
        Plan::Observe => current
            .state
            .is_none()
            .then_some(Transition::CrashLoopObserved),
        Plan::Reap { .. } => {
            // In full mode the `Reaped` event replaces this; a reap already
            // under way is not planned again.
            let entering = matches!(current.state, None | Some(State::Observed));
            (entering && mode == Mode::Report).then_some(Transition::ReapPlanned)
        }
        Plan::Skip { reason, .. } => {
            let previous = current.plan.as_deref().and_then(published_skip);
            (previous != Some(*reason)).then_some(Transition::ReapSkipped(*reason))
        }
        Plan::Recover => Some(Transition::CrashLoopRecovered),
        Plan::RestoreIgnored => Some(Transition::RestoreIgnored),
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
    use crate::model::WorkloadKind;
    use crate::ports::HelmReleasePatch;
    use crate::record::{
        CRASHLOOP_SINCE, HEALTHY_SINCE, HELD_FOR, PLAN, RESTORE, SUSPENDED_BY_REAPER,
    };
    use crate::testing::{
        Call, FakeEvents, FakeTelemetry, FakeWriter, add_pod, block_on, crashlooping_container,
        deployment, healthy_container, helm_release, mins_ago, snapshot,
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

    fn rv_of(call: &Call) -> &str {
        match call {
            Call::PatchMetadata {
                resource_version, ..
            }
            | Call::Scale {
                resource_version, ..
            }
            | Call::SetCronJobSuspend {
                resource_version, ..
            }
            | Call::PatchHelmRelease {
                resource_version, ..
            } => resource_version,
        }
    }

    fn hr_patch(call: &Call) -> &HelmReleasePatch {
        match call {
            Call::PatchHelmRelease { patch, .. } => patch,
            other => panic!("{other:?}"),
        }
    }

    /// A candidate managed by a Kustomization and a HelmRelease.
    fn flux_candidate() -> Snapshot {
        let mut snap = looping(Some("observed"), Some(60), 12);
        let w = &mut snap.workloads[0];
        w.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        w.flux.helm_release = Some(NsName::new("helm", "web"));
        let hr = helm_release("helm", "web");
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        snap
    }

    // US2 (T041)

    #[test]
    fn full_mode_reaps_step_by_step_threading_resource_versions() {
        let r = run(&flux_candidate(), Mode::Full);
        let calls = r.writer.calls();
        assert_eq!(calls.len(), 6, "{calls:#?}");

        // 1. The published record, as in report mode.
        assert_eq!(rv_of(&calls[0]), "1");
        assert_eq!(
            patch(&calls[0]).labels[STATE_LABEL].as_deref(),
            Some("candidate")
        );
        // 2. Record the reap and disable kustomize reconcile in one write.
        let p = patch(&calls[1]);
        assert_eq!(rv_of(&calls[1]), "101");
        assert_eq!(p.labels[STATE_LABEL].as_deref(), Some("reaping"));
        assert_eq!(p.annotations[ORIGINAL_REPLICAS].as_deref(), Some("2"));
        assert_eq!(p.annotations[FLUX_RECONCILE].as_deref(), Some("disabled"));
        assert_eq!(p.annotations[PREVIOUS_RECONCILE].as_deref(), Some("<none>"));
        assert!(
            p.annotations[REASON].as_deref().unwrap().starts_with(
                "container \"api\" CrashLoopBackOff, 12 restarts, crash-looping since "
            )
        );
        // 3. Hold the HelmRelease, then record it on the workload.
        assert_eq!(rv_of(&calls[2]), "1");
        assert_eq!(hr_patch(&calls[2]).suspend, Some(true));
        assert_eq!(rv_of(&calls[3]), "102");
        assert_eq!(
            patch(&calls[3]).annotations[HELMRELEASE].as_deref(),
            Some("helm/web")
        );
        // 4. Scale, 5. mark reaped.
        assert_eq!(
            calls[4],
            Call::Scale {
                workload: WorkloadRef::new(WorkloadKind::Deployment, "app", "web"),
                resource_version: "104".into(),
                replicas: 0,
            }
        );
        let p = patch(&calls[5]);
        assert_eq!(rv_of(&calls[5]), "105");
        assert_eq!(p.labels[STATE_LABEL].as_deref(), Some("reaped"));
        assert!(p.annotations.contains_key(REAPED_AT));

        assert_eq!(r.events.reasons(), ["HelmReleaseHeld", "Reaped"]);
        let events = r.events.events.lock().unwrap();
        assert_eq!(
            events[0].regarding.kind,
            crate::model::ObjectKind::HelmRelease
        );
        assert!(
            events[1].note.starts_with("scaled 2 → 0"),
            "{}",
            events[1].note
        );
        assert_eq!(r.telemetry.rows.lock().unwrap()[0].state, State::Reaped);
        assert_eq!(r.outcome.failures, Vec::new());
    }

    #[test]
    fn a_failed_step_stops_the_reap_and_the_next_cycle_resumes() {
        let snap = flux_candidate();
        let plan = decide(&snap, &Config::default());
        let writer = FakeWriter::new();
        writer.fail_when(|c| matches!(c, Call::Scale { .. }).then_some(WriteError::Conflict));
        let events = FakeEvents::default();
        let telemetry = FakeTelemetry::default();
        let outcome = block_on(execute(
            &snap,
            &plan,
            Mode::Full,
            &writer,
            &events,
            &telemetry,
        ));
        assert_eq!(events.reasons(), ["HelmReleaseHeld", "ReapFailed"]);
        assert_eq!(outcome.failures[0].error, WriteError::Conflict);
        assert_eq!(telemetry.rows.lock().unwrap()[0].state, State::Reaping);
        assert!(telemetry.actions.lock().unwrap().contains(&(
            ActionKind::Reap,
            ActionResult::Error,
            Mode::Full
        )));

        // What the objects look like now: reaping, Flux blocked, replicas 2.
        let mut snap = looping(Some("reaping"), Some(61), 12);
        let w = &mut snap.workloads[0];
        w.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        w.flux.helm_release = Some(NsName::new("helm", "web"));
        w.flux.reconcile_annotation = Some("disabled".into());
        for (k, v) in [
            (ORIGINAL_REPLICAS, "2"),
            (PREVIOUS_RECONCILE, "<none>"),
            (HELMRELEASE, "helm/web"),
        ] {
            w.reaper_annotations.insert(k.into(), v.into());
        }
        let mut hr = helm_release("helm", "web");
        hr.suspended = true;
        hr.reaper_annotations
            .insert(HELD_FOR.into(), r#"["Deployment/app/web"]"#.into());
        hr.reaper_annotations
            .insert(SUSPENDED_BY_REAPER.into(), "true".into());
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        let plan_json = decide(&snap, &Config::default()).decisions[0]
            .plan
            .published_json()
            .unwrap();
        snap.workloads[0]
            .reaper_annotations
            .insert(PLAN.into(), plan_json);

        let r = run(&snap, Mode::Full);
        let calls = r.writer.calls();
        // Nothing done twice: no record, no hold. Replicas > 0 is not a restore.
        assert!(
            matches!(calls[0], Call::Scale { replicas: 0, .. }),
            "{calls:#?}"
        );
        assert_eq!(
            patch(&calls[1]).labels[STATE_LABEL].as_deref(),
            Some("reaped")
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(r.events.reasons(), ["Reaped"]);

        // Report mode leaves an interrupted reap alone and says nothing.
        let r = run(&snap, Mode::Report);
        assert_eq!(r.writer.calls(), Vec::<Call>::new());
        assert_eq!(r.events.reasons(), Vec::<&str>::new());
    }

    // US3 (T050)

    /// A reaped Deployment whose reap disabled kustomize reconcile (it was
    /// unset) and holds HelmRelease helm/web alone.
    fn reaped() -> Snapshot {
        let mut snap = looping(Some("reaped"), Some(90), 12);
        let w = &mut snap.workloads[0];
        w.spec_replicas = Some(0);
        w.flux.kustomization = Some(NsName::new("flux-system", "apps"));
        w.flux.helm_release = Some(NsName::new("helm", "web"));
        w.flux.reconcile_annotation = Some("disabled".into());
        for (k, v) in [
            (ORIGINAL_REPLICAS, "3"),
            (PREVIOUS_RECONCILE, "<none>"),
            (HELMRELEASE, "helm/web"),
            (REASON, "container \"api\" CrashLoopBackOff"),
            (REAPED_AT, "2026-10-10T11:00:00Z"),
        ] {
            w.reaper_annotations.insert(k.into(), v.into());
        }
        let mut hr = helm_release("helm", "web");
        hr.suspended = true;
        hr.reaper_annotations
            .insert(HELD_FOR.into(), r#"["Deployment/app/web"]"#.into());
        hr.reaper_annotations
            .insert(SUSPENDED_BY_REAPER.into(), "true".into());
        snap.helm_releases.insert(hr.helm_release.clone(), hr);
        snap
    }

    #[test]
    fn restore_runs_in_report_mode_and_puts_everything_back() {
        let mut snap = reaped();
        snap.workloads[0]
            .reaper_annotations
            .insert(RESTORE.into(), "true".into());
        let r = run(&snap, Mode::Report);
        let calls = r.writer.calls();
        assert_eq!(calls.len(), 3, "{calls:#?}");
        let hr = hr_patch(&calls[0]);
        assert_eq!(hr.suspend, Some(false));
        assert!(hr.annotations.values().all(Option::is_none));
        assert!(matches!(calls[1], Call::Scale { replicas: 3, .. }));
        let p = patch(&calls[2]);
        assert_eq!(p.labels[STATE_LABEL], None);
        // reconcile was unset before the reap: deleted, not set.
        assert_eq!(p.annotations[FLUX_RECONCILE], None);
        assert_eq!(p.annotations[RESTORE], None);
        assert!(p.annotations.values().all(Option::is_none));
        assert_eq!(r.events.reasons(), ["HelmReleaseReleased", "Restored"]);
        assert!(
            r.events.events.lock().unwrap()[1]
                .note
                .starts_with("requested; 3 replicas")
        );
        assert_eq!(*r.telemetry.rows.lock().unwrap(), Vec::new());
    }

    #[test]
    fn restore_puts_a_previous_reconcile_value_back() {
        let mut snap = reaped();
        snap.workloads[0].spec_replicas = Some(4);
        snap.workloads[0]
            .reaper_annotations
            .insert(PREVIOUS_RECONCILE.into(), "enabled".into());
        let r = run(&snap, Mode::Full);
        let calls = r.writer.calls();
        // Manual scale-up: no scale call.
        assert!(!calls.iter().any(|c| matches!(c, Call::Scale { .. })));
        let p = patch(calls.last().unwrap());
        assert_eq!(p.annotations[FLUX_RECONCILE].as_deref(), Some("enabled"));
        let events = r.events.events.lock().unwrap();
        assert!(
            events[1].note.starts_with("manual-scale-up; 4 replicas"),
            "{}",
            events[1].note
        );
    }

    #[test]
    fn a_failed_restore_step_is_reported_and_retried() {
        let mut snap = reaped();
        snap.workloads[0]
            .reaper_annotations
            .insert(RESTORE.into(), "1".into());
        let plan = decide(&snap, &Config::default());
        let writer = FakeWriter::new();
        writer.fail_when(|c| {
            matches!(c, Call::Scale { .. }).then_some(WriteError::Other("boom".into()))
        });
        let events = FakeEvents::default();
        let telemetry = FakeTelemetry::default();
        let outcome = block_on(execute(
            &snap,
            &plan,
            Mode::Full,
            &writer,
            &events,
            &telemetry,
        ));
        assert_eq!(events.reasons(), ["HelmReleaseReleased", "RestoreFailed"]);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(telemetry.rows.lock().unwrap()[0].state, State::Reaped);
    }

    #[test]
    fn restore_on_a_workload_not_reaped_is_removed_with_an_event() {
        let mut snap = looping(Some("observed"), Some(5), 3);
        snap.workloads[0]
            .reaper_annotations
            .insert(RESTORE.into(), "please".into());
        let r = run(&snap, Mode::Report);
        let calls = r.writer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            patch(&calls[0]).annotations,
            BTreeMap::from([(RESTORE.to_owned(), None)])
        );
        assert_eq!(r.events.reasons(), ["RestoreIgnored"]);
    }

    #[test]
    fn a_stale_hold_is_released() {
        let mut snap = reaped();
        snap.workloads.clear();
        snap.pods.clear();
        let r = run(&snap, Mode::Report);
        let calls = r.writer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(hr_patch(&calls[0]).suspend, Some(false));
        assert_eq!(r.events.reasons(), ["HelmReleaseReleased"]);
    }

    #[test]
    fn mode_off_writes_nothing() {
        let r = run(&looping(None, None, 30), Mode::Off);
        assert_eq!(r.writer.calls(), Vec::<Call>::new());
        assert_eq!(r.events.reasons(), Vec::<&str>::new());
    }
}
