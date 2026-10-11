//! Ports: what the executor needs from the outside world.
//!
//! Plain traits returning `Send` futures, so no runtime is named here. Writes
//! return the object's new `resourceVersion`, which the executor threads into
//! the next write on the same object as a precondition.

use std::collections::BTreeMap;
use std::future::Future;

use crate::config::Mode;
use crate::model::{NsName, ObjectRef, WorkloadRef};
use crate::plan::SkipReason;
use crate::record::{MetadataPatch, State};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    /// The `resourceVersion` precondition failed; retry next cycle.
    #[error("conflict: object changed since it was read")]
    Conflict,
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Other(String),
}

/// A merge patch on a HelmRelease: optionally `spec.suspend`, plus metadata
/// annotations (`None` deletes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelmReleasePatch {
    pub suspend: Option<bool>,
    pub annotations: BTreeMap<String, Option<String>>,
}

pub trait ClusterWriter {
    /// Merge-patch annotations and labels, with a `resourceVersion`
    /// precondition.
    fn patch_metadata(
        &self,
        target: &ObjectRef,
        resource_version: &str,
        patch: &MetadataPatch,
    ) -> impl Future<Output = Result<String, WriteError>> + Send;

    /// Set replicas through the `scale` subresource, with a `resourceVersion`
    /// precondition (the Scale shares its workload's `resourceVersion`).
    fn scale(
        &self,
        workload: &WorkloadRef,
        resource_version: &str,
        replicas: u32,
    ) -> impl Future<Output = Result<String, WriteError>> + Send;

    /// Set a CronJob's `spec.suspend`, with a `resourceVersion` precondition.
    fn set_cronjob_suspend(
        &self,
        workload: &WorkloadRef,
        resource_version: &str,
        suspend: bool,
    ) -> impl Future<Output = Result<String, WriteError>> + Send;

    fn patch_helm_release(
        &self,
        helm_release: &NsName,
        resource_version: &str,
        patch: &HelmReleasePatch,
    ) -> impl Future<Output = Result<String, WriteError>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Normal,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub regarding: ObjectRef,
    pub reason: &'static str,
    pub event_type: EventType,
    pub action: &'static str,
    /// At most 1 KB (contracts/events.md).
    pub note: String,
}

pub trait EventSink {
    fn publish(&self, event: &EventRecord) -> impl Future<Output = Result<(), WriteError>> + Send;
}

/// One workload in a non-`none` state, for the per-workload gauges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadRow {
    pub workload: WorkloadRef,
    pub state: State,
    pub skipped: Option<SkipReason>,
    /// `now - crashloop-since` (or `failing-since`).
    pub crashloop_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionKind {
    Reap,
    Restore,
    Scale,
    KustomizeDisable,
    KustomizeRestore,
    HelmReleaseHold,
    HelmReleaseRelease,
    CronJobSuspend,
    CronJobResume,
    /// The reaper's own labels and annotations. Counted only on failure.
    Record,
}

impl ActionKind {
    /// Every variant, in declaration order (the `action` label values).
    pub const ALL: [ActionKind; 10] = [
        ActionKind::Reap,
        ActionKind::Restore,
        ActionKind::Scale,
        ActionKind::KustomizeDisable,
        ActionKind::KustomizeRestore,
        ActionKind::HelmReleaseHold,
        ActionKind::HelmReleaseRelease,
        ActionKind::CronJobSuspend,
        ActionKind::CronJobResume,
        ActionKind::Record,
    ];

    /// The variant's index in `ALL`. Exhaustive, so a new variant does not
    /// compile until it is given the next position here, and at the same
    /// position in `ALL`.
    const fn position(self) -> usize {
        match self {
            ActionKind::Reap => 0,
            ActionKind::Restore => 1,
            ActionKind::Scale => 2,
            ActionKind::KustomizeDisable => 3,
            ActionKind::KustomizeRestore => 4,
            ActionKind::HelmReleaseHold => 5,
            ActionKind::HelmReleaseRelease => 6,
            ActionKind::CronJobSuspend => 7,
            ActionKind::CronJobResume => 8,
            ActionKind::Record => 9,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Reap => "reap",
            ActionKind::Restore => "restore",
            ActionKind::Scale => "scale",
            ActionKind::KustomizeDisable => "kustomize_disable",
            ActionKind::KustomizeRestore => "kustomize_restore",
            ActionKind::HelmReleaseHold => "helmrelease_hold",
            ActionKind::HelmReleaseRelease => "helmrelease_release",
            ActionKind::CronJobSuspend => "cronjob_suspend",
            ActionKind::CronJobResume => "cronjob_resume",
            ActionKind::Record => "record",
        }
    }
}

const _: () = {
    let mut i = 0;
    while i < ActionKind::ALL.len() {
        assert!(ActionKind::ALL[i].position() == i);
        i += 1;
    }
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionResult {
    Success,
    Error,
    /// Report mode: the action was planned, not performed.
    Planned,
}

impl ActionResult {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ActionResult::Success => "success",
            ActionResult::Error => "error",
            ActionResult::Planned => "planned",
        }
    }
}

/// Why the reaper is or is not ready, as the `Ready` condition on the
/// `ReaperConfig` reports it (contracts/config.md, `status.conditions`).
///
/// Plain data: the HTTP adapter computes it from its health state, the kube
/// adapter writes it. A valid config is not part of it; that shows on `Valid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Readiness {
    /// A cycle finished within the last 3 cycle intervals.
    CycleCompleted,
    /// The last cycle finished 3 or more cycle intervals ago.
    Stalled,
    /// The caches have not synced, or no cycle has finished yet.
    CachesNotSynced,
}

impl Readiness {
    /// Every variant, in declaration order.
    pub const ALL: [Readiness; 3] = [
        Readiness::CycleCompleted,
        Readiness::Stalled,
        Readiness::CachesNotSynced,
    ];

    /// The variant's index in `ALL`; exhaustive, as for [`ActionKind`].
    const fn position(self) -> usize {
        match self {
            Readiness::CycleCompleted => 0,
            Readiness::Stalled => 1,
            Readiness::CachesNotSynced => 2,
        }
    }

    /// The condition's `reason`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Readiness::CycleCompleted => "CycleCompleted",
            Readiness::Stalled => "Stalled",
            Readiness::CachesNotSynced => "CachesNotSynced",
        }
    }

    /// Whether the condition's status is `True`.
    #[must_use]
    pub fn is_ready(self) -> bool {
        matches!(self, Readiness::CycleCompleted)
    }
}

const _: () = {
    let mut i = 0;
    while i < Readiness::ALL.len() {
        assert!(Readiness::ALL[i].position() == i);
        i += 1;
    }
};

/// Synchronous: recording a metric never waits.
pub trait Telemetry {
    /// Replace every per-workload series with `rows` (stale ones disappear).
    fn workloads(&self, rows: &[WorkloadRow]);
    fn action(&self, kind: ActionKind, result: ActionResult, mode: Mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_labels_are_distinct() {
        let labels: std::collections::BTreeSet<_> =
            ActionKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(labels.len(), ActionKind::ALL.len());
        assert_eq!(ActionKind::Record.as_str(), "record");
    }
}
