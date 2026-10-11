//! `EventSink` contract: an `events.k8s.io/v1` Event per publish.

use http::{Method, Request, Response};
use kube::Client;
use kube::client::Body;
use reaper_core::model::{NsName, ObjectKind, ObjectRef, WorkloadKind, WorkloadRef};
use reaper_core::ports::{EventRecord, EventSink, EventType, WriteError};
use reaper_kube::events::KubeEventSink;
use serde_json::{Value, json};
use tower_test::mock::{self, Handle};

/// A request the mock API server saw.
type Seen = (Method, String, Value);

fn sink(status: u16) -> (KubeEventSink, tokio::task::JoinHandle<Seen>) {
    let (s, server) = sink_n(vec![status]);
    let server = tokio::spawn(async move { server.await.unwrap().remove(0) });
    (s, server)
}

/// A sink over a mock API server that answers one request per status, echoing
/// the body back on success.
fn sink_n(statuses: Vec<u16>) -> (KubeEventSink, tokio::task::JoinHandle<Vec<Seen>>) {
    let (service, mut handle): (_, Handle<Request<Body>, Response<Body>>) = mock::pair();
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for status in statuses {
            let (req, send) = handle.next_request().await.expect("a request");
            let (parts, body) = req.into_parts();
            let body: Value = serde_json::from_slice(&body.collect_bytes().await.unwrap()).unwrap();
            let reply = if status < 300 {
                body.clone()
            } else {
                json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": status})
            };
            send.send_response(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                    .unwrap(),
            );
            seen.push((parts.method, parts.uri.path().to_owned(), body));
        }
        seen
    });
    (
        KubeEventSink::new(Client::new(service, "default"), "k8s-reaper-7d9f-abcde"),
        server,
    )
}

fn applied(note: &str) -> EventRecord {
    EventRecord {
        regarding: ObjectRef {
            kind: ObjectKind::ReaperConfig,
            namespace: None,
            name: "default".into(),
        },
        reason: "ConfigApplied",
        event_type: EventType::Normal,
        action: "Validate",
        note: note.into(),
    }
}

#[tokio::test]
async fn publish_creates_an_event_regarding_the_workload() {
    let (s, server) = sink(201);
    let event = EventRecord {
        regarding: WorkloadRef::new(WorkloadKind::Deployment, "shop", "web").object_ref(),
        reason: "Reaped",
        event_type: EventType::Warning,
        action: "ScaleDown",
        note: "scaled to 0 from 2".into(),
    };
    assert_eq!(s.publish(&event).await, Ok(()));
    let (method, path, body) = server.await.unwrap();
    assert_eq!(method, Method::POST);
    assert_eq!(path, "/apis/events.k8s.io/v1/namespaces/shop/events");
    assert_eq!(body["reason"], "Reaped");
    assert_eq!(body["type"], "Warning");
    assert_eq!(body["action"], "ScaleDown");
    assert_eq!(body["note"], "scaled to 0 from 2");
    assert_eq!(body["reportingController"], "reaper.boop.ninja/k8s-reaper");
    assert_eq!(body["reportingInstance"], "k8s-reaper-7d9f-abcde");
    assert_eq!(
        body["regarding"],
        json!({"apiVersion": "apps/v1", "kind": "Deployment", "namespace": "shop", "name": "web"})
    );
}

#[tokio::test]
async fn publish_truncates_the_note_and_maps_errors() {
    let (s, server) = sink(404);
    let event = EventRecord {
        regarding: ObjectRef::helm_release(&NsName::new("helm", "web")),
        reason: "HelmReleaseHeld",
        event_type: EventType::Warning,
        action: "Suspend",
        note: "x".repeat(2000),
    };
    assert_eq!(s.publish(&event).await, Err(WriteError::NotFound));
    let (_, path, body) = server.await.unwrap();
    assert_eq!(path, "/apis/events.k8s.io/v1/namespaces/helm/events");
    assert_eq!(body["note"].as_str().unwrap().len(), 1024);
    assert_eq!(body["regarding"]["apiVersion"], "helm.toolkit.fluxcd.io/v2");
}

/// G18: a repeat continues the series only when the note is the same; a new
/// note is a new Event, so `kubectl events` never shows a stale one.
#[tokio::test]
async fn a_new_note_is_a_new_event_and_a_repeat_continues_the_series() {
    let (s, server) = sink_n(vec![201, 201, 200]);
    s.publish(&applied("generation 6 is active")).await.unwrap();
    s.publish(&applied("generation 8 is active")).await.unwrap();
    s.publish(&applied("generation 8 is active")).await.unwrap();
    let seen = server.await.unwrap();

    let (first, second, repeat) = (&seen[0], &seen[1], &seen[2]);
    assert_eq!(first.0, Method::POST);
    assert_eq!(first.1, "/apis/events.k8s.io/v1/namespaces/default/events");
    assert_eq!(second.0, Method::POST);
    assert_eq!(second.2["note"], "generation 8 is active");
    assert_ne!(first.2["metadata"]["name"], second.2["metadata"]["name"]);

    assert_eq!(repeat.0, Method::PATCH);
    let name = second.2["metadata"]["name"].as_str().unwrap();
    assert_eq!(
        repeat.1,
        format!("/apis/events.k8s.io/v1/namespaces/default/events/{name}")
    );
    assert_eq!(repeat.2["note"], "generation 8 is active");
    assert_eq!(repeat.2["series"]["count"], 2);
}
