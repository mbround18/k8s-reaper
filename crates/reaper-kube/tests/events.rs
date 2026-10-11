//! `EventSink` contract: an `events.k8s.io/v1` Event per publish.

use http::{Method, Request, Response};
use kube::Client;
use kube::client::Body;
use reaper_core::model::{NsName, ObjectRef, WorkloadKind, WorkloadRef};
use reaper_core::ports::{EventRecord, EventSink, EventType, WriteError};
use reaper_kube::events::KubeEventSink;
use serde_json::{Value, json};
use tower_test::mock::{self, Handle};

fn sink(
    status: u16,
) -> (
    KubeEventSink,
    tokio::task::JoinHandle<(Method, String, Value)>,
) {
    let (service, mut handle): (_, Handle<Request<Body>, Response<Body>>) = mock::pair();
    let server = tokio::spawn(async move {
        let (req, send) = handle.next_request().await.expect("one request");
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
        (parts.method, parts.uri.path().to_owned(), body)
    });
    (
        KubeEventSink::new(Client::new(service, "default"), "k8s-reaper-7d9f-abcde"),
        server,
    )
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
