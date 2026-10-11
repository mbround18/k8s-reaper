//! `EventSink` over the `events.k8s.io/v1` Recorder (contracts/events.md).

use std::future::Future;

use k8s_openapi::api::core::v1::ObjectReference;
use kube::Client;
use kube::runtime::events::{self, Event, Recorder, Reporter};
use reaper_core::model::ObjectRef;
use reaper_core::ports::{EventRecord, EventSink, EventType, WriteError};

use crate::snapshot::Uids;
use crate::writer::{api_resource, write_error};

pub const REPORTING_CONTROLLER: &str = "reaper.boop.ninja/k8s-reaper";

/// Event notes are capped at 1 KB.
pub const NOTE_LIMIT: usize = 1024;

#[derive(Clone)]
pub struct KubeEventSink {
    recorder: Recorder,
    uids: Option<Uids>,
}

impl KubeEventSink {
    /// `instance` is the reporting instance, normally the pod name.
    #[must_use]
    pub fn new(client: Client, instance: impl Into<String>) -> Self {
        let reporter = Reporter {
            controller: REPORTING_CONTROLLER.into(),
            instance: Some(instance.into()),
        };
        KubeEventSink {
            recorder: Recorder::new(client, reporter),
            uids: None,
        }
    }

    /// Fill `regarding.uid` from `uids`, so `kubectl describe` attaches the
    /// Events to the object.
    #[must_use]
    pub fn with_uids(mut self, uids: Uids) -> Self {
        self.uids = Some(uids);
        self
    }
}

/// The `regarding` reference. Events about cluster-scoped objects land in
/// `default`: the API server rejects any other namespace for them.
#[must_use]
pub fn object_reference(target: &ObjectRef, uid: Option<String>) -> ObjectReference {
    ObjectReference {
        api_version: Some(api_resource(target.kind).api_version),
        kind: Some(target.kind.as_str().to_owned()),
        namespace: target.namespace.clone(),
        name: Some(target.name.clone()),
        uid,
        ..ObjectReference::default()
    }
}

/// The longest prefix of `note` within [`NOTE_LIMIT`] bytes that ends on a
/// char boundary.
#[must_use]
pub fn truncate_note(note: &str) -> &str {
    if note.len() <= NOTE_LIMIT {
        return note;
    }
    let end = (0..=NOTE_LIMIT)
        .rev()
        .find(|&i| note.is_char_boundary(i))
        .unwrap_or(0);
    &note[..end]
}

fn event(record: &EventRecord) -> Event {
    Event {
        type_: match record.event_type {
            EventType::Normal => events::EventType::Normal,
            EventType::Warning => events::EventType::Warning,
        },
        reason: record.reason.to_owned(),
        note: Some(truncate_note(&record.note).to_owned()),
        action: record.action.to_owned(),
        secondary: None,
    }
}

impl EventSink for KubeEventSink {
    fn publish(
        &self,
        event_record: &EventRecord,
    ) -> impl Future<Output = Result<(), WriteError>> + Send {
        let ev = event(event_record);
        let uid = self
            .uids
            .as_ref()
            .and_then(|u| u.get(&event_record.regarding));
        let reference = object_reference(&event_record.regarding, uid);
        async move {
            self.recorder
                .publish(&ev, &reference)
                .await
                .map_err(write_error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reaper_core::model::{NsName, ObjectKind, WorkloadKind, WorkloadRef};

    fn reference(kind: ObjectKind, ns: Option<&str>) -> ObjectReference {
        object_reference(
            &ObjectRef {
                kind,
                namespace: ns.map(str::to_owned),
                name: "x".into(),
            },
            None,
        )
    }

    #[test]
    fn api_versions_per_kind() {
        for (kind, api_version) in [
            (ObjectKind::Deployment, "apps/v1"),
            (ObjectKind::StatefulSet, "apps/v1"),
            (ObjectKind::DaemonSet, "apps/v1"),
            (ObjectKind::CronJob, "batch/v1"),
            (ObjectKind::Pod, "v1"),
            (ObjectKind::Namespace, "v1"),
            (ObjectKind::HelmRelease, "helm.toolkit.fluxcd.io/v2"),
            (ObjectKind::ReaperConfig, "reaper.boop.ninja/v1alpha1"),
        ] {
            let r = reference(kind, None);
            assert_eq!(r.api_version.as_deref(), Some(api_version), "{kind:?}");
            assert_eq!(r.kind.as_deref(), Some(kind.as_str()));
        }
    }

    #[test]
    fn reference_carries_namespace_name_and_uid() {
        let w = WorkloadRef::new(WorkloadKind::CronJob, "batch", "nightly").object_ref();
        assert_eq!(
            object_reference(&w, Some("u-1".into())),
            ObjectReference {
                api_version: Some("batch/v1".into()),
                kind: Some("CronJob".into()),
                namespace: Some("batch".into()),
                name: Some("nightly".into()),
                uid: Some("u-1".into()),
                ..ObjectReference::default()
            }
        );
        let hr = object_reference(&ObjectRef::helm_release(&NsName::new("helm", "web")), None);
        assert_eq!(hr.namespace.as_deref(), Some("helm"));
        assert_eq!(hr.uid, None);
        assert_eq!(reference(ObjectKind::Namespace, None).namespace, None);
    }

    #[test]
    fn short_notes_are_kept() {
        assert_eq!(truncate_note(""), "");
        let exact = "a".repeat(NOTE_LIMIT);
        assert_eq!(truncate_note(&exact), exact);
    }

    #[test]
    fn long_notes_are_cut_on_a_char_boundary() {
        let ascii = "a".repeat(NOTE_LIMIT + 10);
        assert_eq!(truncate_note(&ascii).len(), NOTE_LIMIT);
        // 'é' is two bytes: an odd prefix pushes byte 1024 into the middle of one.
        let mixed = format!("a{}", "é".repeat(NOTE_LIMIT));
        let cut = truncate_note(&mixed);
        assert_eq!(cut.len(), NOTE_LIMIT - 1);
        assert!(mixed.starts_with(cut));
    }

    #[test]
    fn record_maps_onto_a_kube_event() {
        let record = EventRecord {
            regarding: WorkloadRef::new(WorkloadKind::Deployment, "shop", "web").object_ref(),
            reason: "ReapPlanned",
            event_type: EventType::Warning,
            action: "Plan",
            note: "would scale to 0".into(),
        };
        let ev = event(&record);
        assert_eq!(ev.type_, events::EventType::Warning);
        assert_eq!(ev.reason, "ReapPlanned");
        assert_eq!(ev.action, "Plan");
        assert_eq!(ev.note.as_deref(), Some("would scale to 0"));
        assert!(ev.secondary.is_none());
    }
}
