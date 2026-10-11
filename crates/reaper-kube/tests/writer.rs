//! `ClusterWriter` contract: the HTTP requests the kube adapter sends.

use std::collections::BTreeMap;

use http::{Method, Request, Response};
use kube::Client;
use kube::client::Body;
use reaper_core::model::{NsName, ObjectKind, ObjectRef, WorkloadKind, WorkloadRef};
use reaper_core::ports::{ClusterWriter, HelmReleasePatch, WriteError};
use reaper_core::record::MetadataPatch;
use reaper_kube::writer::KubeWriter;
use serde_json::{Value, json};
use tower_test::mock::{self, Handle};

struct Seen {
    method: Method,
    path: String,
    query: String,
    content_type: String,
    body: Value,
}

/// A writer over a mock API server that answers one request with `status`
/// and `reply`, and hands back what it saw.
fn writer(status: u16, reply: Value) -> (KubeWriter, tokio::task::JoinHandle<Seen>) {
    let (service, mut handle): (_, Handle<Request<Body>, Response<Body>>) = mock::pair();
    let server = tokio::spawn(async move {
        let (req, send) = handle.next_request().await.expect("one request");
        let (parts, body) = req.into_parts();
        let body = body.collect_bytes().await.unwrap();
        send.send_response(
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                .unwrap(),
        );
        Seen {
            method: parts.method,
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().unwrap_or_default().to_owned(),
            content_type: parts
                .headers
                .get("content-type")
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        }
    });
    (KubeWriter::new(Client::new(service, "default")), server)
}

fn object(api_version: &str, kind: &str, rv: &str) -> Value {
    json!({
        "apiVersion": api_version,
        "kind": kind,
        "metadata": {"name": "web", "namespace": "shop", "resourceVersion": rv},
    })
}

fn status(code: u16, reason: &str) -> Value {
    json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": reason,
        "reason": reason,
        "code": code,
    })
}

fn deployment() -> ObjectRef {
    WorkloadRef::new(WorkloadKind::Deployment, "shop", "web").object_ref()
}

fn patch() -> MetadataPatch {
    MetadataPatch {
        annotations: BTreeMap::from([
            ("reaper.boop.ninja/plan".to_owned(), Some("{}".to_owned())),
            ("reaper.boop.ninja/healthy-since".to_owned(), None),
        ]),
        labels: BTreeMap::from([(
            "reaper.boop.ninja/state".to_owned(),
            Some("candidate".to_owned()),
        )]),
    }
}

fn assert_merge_patch(seen: &Seen, path: &str) {
    assert_eq!(seen.method, Method::PATCH);
    assert_eq!(seen.path, path);
    assert_eq!(seen.content_type, "application/merge-patch+json");
    assert!(
        seen.query
            .split('&')
            .any(|q| q == "fieldManager=k8s-reaper"),
        "query {:?}",
        seen.query
    );
}

#[tokio::test]
async fn patch_metadata_sends_a_merge_patch_with_a_precondition() {
    let (w, server) = writer(200, object("apps/v1", "Deployment", "43"));
    let rv = w.patch_metadata(&deployment(), "42", &patch()).await;
    let seen = server.await.unwrap();
    assert_eq!(rv, Ok("43".to_owned()));
    assert_merge_patch(&seen, "/apis/apps/v1/namespaces/shop/deployments/web");
    assert_eq!(
        seen.body,
        json!({
            "metadata": {
                "resourceVersion": "42",
                "annotations": {
                    "reaper.boop.ninja/plan": "{}",
                    "reaper.boop.ninja/healthy-since": null,
                },
                "labels": {"reaper.boop.ninja/state": "candidate"},
            }
        })
    );
}

#[tokio::test]
async fn patch_metadata_omits_empty_label_and_annotation_maps() {
    let (w, server) = writer(200, object("v1", "Pod", "8"));
    let only_labels = MetadataPatch {
        annotations: BTreeMap::new(),
        labels: BTreeMap::from([("reaper.boop.ninja/state".to_owned(), None)]),
    };
    let target = ObjectRef {
        kind: ObjectKind::Pod,
        namespace: Some("shop".into()),
        name: "web-0".into(),
    };
    assert_eq!(
        w.patch_metadata(&target, "7", &only_labels).await,
        Ok("8".to_owned())
    );
    let seen = server.await.unwrap();
    assert_merge_patch(&seen, "/api/v1/namespaces/shop/pods/web-0");
    assert_eq!(
        seen.body,
        json!({"metadata": {"resourceVersion": "7", "labels": {"reaper.boop.ninja/state": null}}})
    );
}

#[tokio::test]
async fn every_object_kind_has_a_path() {
    let cases = [
        (
            ObjectKind::StatefulSet,
            Some("shop"),
            "/apis/apps/v1/namespaces/shop/statefulsets/web",
        ),
        (
            ObjectKind::DaemonSet,
            Some("shop"),
            "/apis/apps/v1/namespaces/shop/daemonsets/web",
        ),
        (
            ObjectKind::CronJob,
            Some("shop"),
            "/apis/batch/v1/namespaces/shop/cronjobs/web",
        ),
        (
            ObjectKind::HelmRelease,
            Some("shop"),
            "/apis/helm.toolkit.fluxcd.io/v2/namespaces/shop/helmreleases/web",
        ),
        (ObjectKind::Namespace, None, "/api/v1/namespaces/web"),
        (
            ObjectKind::ReaperConfig,
            None,
            "/apis/reaper.boop.ninja/v1alpha1/reaperconfigs/web",
        ),
    ];
    for (kind, ns, path) in cases {
        let (w, server) = writer(200, object("v1", kind.as_str(), "2"));
        let target = ObjectRef {
            kind,
            namespace: ns.map(str::to_owned),
            name: "web".into(),
        };
        assert_eq!(
            w.patch_metadata(&target, "1", &patch()).await,
            Ok("2".to_owned())
        );
        assert_merge_patch(&server.await.unwrap(), path);
    }
}

#[tokio::test]
async fn conflict_and_not_found_are_mapped() {
    for (code, reason, expected) in [
        (409, "Conflict", WriteError::Conflict),
        (404, "NotFound", WriteError::NotFound),
    ] {
        let (w, server) = writer(code, status(code, reason));
        assert_eq!(
            w.patch_metadata(&deployment(), "42", &patch()).await,
            Err(expected)
        );
        server.await.unwrap();
    }
    let (w, server) = writer(403, status(403, "Forbidden"));
    let err = w
        .patch_metadata(&deployment(), "42", &patch())
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(matches!(err, WriteError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn scale_patches_the_scale_subresource() {
    let reply = json!({
        "apiVersion": "autoscaling/v1",
        "kind": "Scale",
        "metadata": {"name": "db", "namespace": "shop", "resourceVersion": "11"},
        "spec": {"replicas": 0},
    });
    let (w, server) = writer(200, reply);
    let sts = WorkloadRef::new(WorkloadKind::StatefulSet, "shop", "db");
    assert_eq!(w.scale(&sts, "10", 0).await, Ok("11".to_owned()));
    let seen = server.await.unwrap();
    assert_merge_patch(&seen, "/apis/apps/v1/namespaces/shop/statefulsets/db/scale");
    assert_eq!(
        seen.body,
        json!({"metadata": {"resourceVersion": "10"}, "spec": {"replicas": 0}})
    );
}

#[tokio::test]
async fn scale_conflict_is_mapped() {
    let (w, server) = writer(409, status(409, "Conflict"));
    let web = WorkloadRef::new(WorkloadKind::Deployment, "shop", "web");
    assert_eq!(w.scale(&web, "10", 0).await, Err(WriteError::Conflict));
    server.await.unwrap();
}

#[tokio::test]
async fn scale_refuses_kinds_without_a_scale_subresource() {
    let (w, server) = writer(200, json!({}));
    let ds = WorkloadRef::new(WorkloadKind::DaemonSet, "shop", "agent");
    assert!(matches!(
        w.scale(&ds, "1", 0).await,
        Err(WriteError::Other(_))
    ));
    server.abort();
}

#[tokio::test]
async fn set_cronjob_suspend_carries_a_precondition() {
    let (w, server) = writer(200, object("batch/v1", "CronJob", "6"));
    let cj = WorkloadRef::new(WorkloadKind::CronJob, "shop", "nightly");
    assert_eq!(
        w.set_cronjob_suspend(&cj, "5", true).await,
        Ok("6".to_owned())
    );
    let seen = server.await.unwrap();
    assert_merge_patch(&seen, "/apis/batch/v1/namespaces/shop/cronjobs/nightly");
    assert_eq!(
        seen.body,
        json!({"metadata": {"resourceVersion": "5"}, "spec": {"suspend": true}})
    );
}

#[tokio::test]
async fn patch_helm_release_sets_suspend_and_annotations() {
    let (w, server) = writer(
        200,
        object("helm.toolkit.fluxcd.io/v2", "HelmRelease", "21"),
    );
    let hr = NsName::new("helm", "web");
    let p = HelmReleasePatch {
        suspend: Some(false),
        annotations: BTreeMap::from([
            ("reaper.boop.ninja/held-for".to_owned(), None),
            (
                "reaper.boop.ninja/suspended-by-reaper".to_owned(),
                Some("false".to_owned()),
            ),
        ]),
    };
    assert_eq!(
        w.patch_helm_release(&hr, "20", &p).await,
        Ok("21".to_owned())
    );
    let seen = server.await.unwrap();
    assert_merge_patch(
        &seen,
        "/apis/helm.toolkit.fluxcd.io/v2/namespaces/helm/helmreleases/web",
    );
    assert_eq!(
        seen.body,
        json!({
            "metadata": {
                "resourceVersion": "20",
                "annotations": {
                    "reaper.boop.ninja/held-for": null,
                    "reaper.boop.ninja/suspended-by-reaper": "false",
                },
            },
            "spec": {"suspend": false},
        })
    );
}

#[tokio::test]
async fn patch_helm_release_without_suspend_leaves_spec_alone() {
    let (w, server) = writer(200, object("helm.toolkit.fluxcd.io/v2", "HelmRelease", "3"));
    let p = HelmReleasePatch {
        suspend: None,
        annotations: BTreeMap::from([("reaper.boop.ninja/held-for".to_owned(), Some("[]".into()))]),
    };
    assert_eq!(
        w.patch_helm_release(&NsName::new("helm", "web"), "2", &p)
            .await,
        Ok("3".to_owned())
    );
    assert_eq!(
        server.await.unwrap().body,
        json!({"metadata": {"resourceVersion": "2", "annotations": {"reaper.boop.ninja/held-for": "[]"}}})
    );
}
