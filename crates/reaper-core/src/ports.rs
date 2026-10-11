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
}

impl ActionKind {
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
        }
    }
}

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

/// Synchronous: recording a metric never waits.
pub trait Telemetry {
    /// Replace every per-workload series with `rows` (stale ones disappear).
    fn workloads(&self, rows: &[WorkloadRow]);
    fn action(&self, kind: ActionKind, result: ActionResult, mode: Mode);
}
