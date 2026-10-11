//! `EventSink` over `events.k8s.io/v1` (contracts/events.md).
//!
//! kube's `Recorder` folds a repeat of the same object, reason, action and
//! type into the first Event's series and keeps that Event's note, so a later
//! transition with a different note would show the old one (G18). Here the
//! note is part of the key, as in client-go: an identical repeat within
//! [`SERIES_TTL`] bumps the series, anything else is a new Event.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::api::events::v1::{Event, EventSeries};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::{Client, ResourceExt as _};
use reaper_core::model::ObjectRef;
use reaper_core::ports::{EventRecord, EventSink, EventType, WriteError};

use crate::snapshot::Uids;
use crate::writer::{api_resource, write_error};

pub const REPORTING_CONTROLLER: &str = "reaper.boop.ninja/k8s-reaper";

/// Event notes are capped at 1 KB.
pub const NOTE_LIMIT: usize = 1024;

/// How long an identical repeat continues an Event's series (kube's value).
pub const SERIES_TTL: SignedDuration = SignedDuration::from_mins(6);

/// What makes two publishes the same Event.
type Key = (ObjectRef, &'static str, &'static str, bool, String);

#[derive(Clone)]
pub struct KubeEventSink {
    client: Client,
    instance: String,
    uids: Option<Uids>,
    recent: Arc<Mutex<BTreeMap<Key, Event>>>,
}

impl KubeEventSink {
    /// `instance` is the reporting instance, normally the pod name.
    #[must_use]
    pub fn new(client: Client, instance: impl Into<String>) -> Self {
        KubeEventSink {
            client,
            instance: instance.into(),
            uids: None,
            recent: Arc::default(),
        }
    }

    /// Fill `regarding.uid` from `uids`, so `kubectl describe` attaches the
    /// Events to the object.
    #[must_use]
    pub fn with_uids(mut self, uids: Uids) -> Self {
        self.uids = Some(uids);
        self
    }

    /// The Event to write for `record` at `now`: the recent identical one with
    /// its series bumped, or a new one.
    fn next(&self, record: &EventRecord, key: &Key, now: Timestamp) -> Event {
        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        recent.retain(|_, e| last_seen(e).is_some_and(|t| t + SERIES_TTL > now));
        if let Some(previous) = recent.get(key) {
            let mut event = previous.clone();
            event.series = Some(EventSeries {
                count: previous.series.as_ref().map_or(2, |s| s.count + 1),
                last_observed_time: MicroTime(now),
            });
            return event;
        }
        let uid = self.uids.as_ref().and_then(|u| u.get(&record.regarding));
        new_event(
            record,
            object_reference(&record.regarding, uid),
            &self.instance,
            now,
        )
    }
}

fn last_seen(e: &Event) -> Option<Timestamp> {
    e.series
        .as_ref()
        .map(|s| s.last_observed_time.0)
        .or(e.event_time.as_ref().map(|t| t.0))
}

fn new_event(
    record: &EventRecord,
    regarding: ObjectReference,
    instance: &str,
    now: Timestamp,
) -> Event {
    Event {
        metadata: ObjectMeta {
            name: Some(format!(
                "{}.{:x}",
                record.regarding.name,
                now.as_nanosecond()
            )),
            namespace: Some(namespace(&regarding).to_owned()),
            ..ObjectMeta::default()
        },
        action: Some(record.action.to_owned()),
        reason: Some(record.reason.to_owned()),
        type_: Some(event_type(record.event_type).to_owned()),
        note: Some(truncate_note(&record.note).to_owned()),
        event_time: Some(MicroTime(now)),
        regarding: Some(regarding),
        reporting_controller: Some(REPORTING_CONTROLLER.to_owned()),
        reporting_instance: Some(instance.to_owned()),
        ..Event::default()
    }
}

fn event_type(t: EventType) -> &'static str {
    match t {
        EventType::Normal => "Normal",
        EventType::Warning => "Warning",
    }
}

/// Events about cluster-scoped objects live in `default`.
fn namespace(regarding: &ObjectReference) -> &str {
    regarding.namespace.as_deref().unwrap_or("default")
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

impl EventSink for KubeEventSink {
    fn publish(
        &self,
        event_record: &EventRecord,
    ) -> impl Future<Output = Result<(), WriteError>> + Send {
        let key: Key = (
            event_record.regarding.clone(),
            event_record.reason,
            event_record.action,
            event_record.event_type == EventType::Warning,
            truncate_note(&event_record.note).to_owned(),
        );
        let event = self.next(event_record, &key, Timestamp::now());
        let api: Api<Event> = Api::namespaced(
            self.client.clone(),
            event.metadata.namespace.as_deref().unwrap_or("default"),
        );
        let recent = Arc::clone(&self.recent);
        async move {
            if event.series.is_some() {
                api.patch(
                    &event.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(&event),
                )
                .await
            } else {
                api.create(&PostParams::default(), &event).await
            }
            .map_err(write_error)?;
            recent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, event);
            Ok(())
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
        let now = Timestamp::from_second(1_000).unwrap();
        let ev = new_event(
            &record,
            object_reference(&record.regarding, None),
            "pod-1",
            now,
        );
        assert_eq!(ev.type_.as_deref(), Some("Warning"));
        assert_eq!(ev.reason.as_deref(), Some("ReapPlanned"));
        assert_eq!(ev.action.as_deref(), Some("Plan"));
        assert_eq!(ev.note.as_deref(), Some("would scale to 0"));
        assert_eq!(ev.metadata.namespace.as_deref(), Some("shop"));
        assert_eq!(ev.reporting_instance.as_deref(), Some("pod-1"));
        assert!(ev.related.is_none());
    }
}
