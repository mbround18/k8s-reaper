//! `ConfigSource` contract: what it reads, what it keeps, what it writes
//! (T061, FR-024, FR-026).

use http::{Method, Request, Response};
use kube::Client;
use kube::client::Body;
use reaper_core::Timestamp;
use reaper_core::config::Mode;
use reaper_core::config_state::{ConfigChange, Counts};
use reaper_kube::config_watch::ConfigSource;
use serde_json::{Value, json};
use tower_test::mock::{self, Handle};

struct Seen {
    method: Method,
    path: String,
    content_type: String,
    body: Value,
}

/// A source over a mock API server that answers each request with the next
/// reply, and hands back what it saw once every reply was used.
fn source(replies: Vec<(u16, Value)>) -> (ConfigSource, tokio::task::JoinHandle<Vec<Seen>>) {
    let (service, mut handle): (_, Handle<Request<Body>, Response<Body>>) = mock::pair();
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (status, reply) in replies {
            let (req, send) = handle.next_request().await.expect("a request");
            let (parts, body) = req.into_parts();
            let body = body.collect_bytes().await.unwrap();
            send.send_response(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                    .unwrap(),
            );
            seen.push(Seen {
                method: parts.method,
                path: parts.uri.path().to_owned(),
                content_type: parts
                    .headers
                    .get("content-type")
                    .map(|v| v.to_str().unwrap().to_owned())
                    .unwrap_or_default(),
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            });
        }
        seen
    });
    (
        ConfigSource::new(Client::new(service, "default"), "default"),
        server,
    )
}

fn config(generation: i64, spec: Value) -> (u16, Value) {
    let mut object = json!({
        "apiVersion": "reaper.boop.ninja/v1alpha1",
        "kind": "ReaperConfig",
        "metadata": {"name": "default", "generation": generation, "resourceVersion": "1"},
    });
    object["spec"] = spec;
    (200, object)
}

fn not_found() -> (u16, Value) {
    (
        404,
        json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "NotFound", "code": 404,
            "message": "reaperconfigs.reaper.boop.ninja \"default\" not found",
        }),
    )
}

const PATH: &str = "/apis/reaper.boop.ninja/v1alpha1/reaperconfigs/default";

#[tokio::test]
async fn a_new_valid_generation_becomes_active() {
    let (mut src, server) = source(vec![
        config(1, json!({"mode": "full"})),
        config(1, json!({"mode": "full"})),
    ]);
    assert_eq!(
        src.refresh().await.unwrap(),
        Some(ConfigChange::Applied { generation: 1 })
    );
    assert_eq!(src.refresh().await.unwrap(), None);
    assert_eq!(src.state().active().mode, Mode::Full);
    let seen = server.await.unwrap();
    assert_eq!(
        (&seen[0].method, seen[0].path.as_str()),
        (&Method::GET, PATH)
    );
}

#[tokio::test]
async fn an_invalid_generation_keeps_the_last_valid_config() {
    let (mut src, server) = source(vec![
        config(1, json!({"mode": "full"})),
        config(2, json!({"mode": "report", "cycleInterval": "5s"})),
    ]);
    src.refresh().await.unwrap();
    let change = src.refresh().await.unwrap();
    assert!(
        matches!(change, Some(ConfigChange::Invalid { generation: 2, .. })),
        "{change:?}"
    );
    assert_eq!(src.state().active().mode, Mode::Full);
    assert_eq!(src.state().active_generation(), Some(1));
    server.await.unwrap();
}

#[tokio::test]
async fn deletion_falls_back_to_the_defaults() {
    let (mut src, server) = source(vec![config(1, json!({"mode": "full"})), not_found()]);
    src.refresh().await.unwrap();
    assert_eq!(src.refresh().await.unwrap(), Some(ConfigChange::Removed));
    assert_eq!(src.state().active().mode, Mode::Report);
    server.await.unwrap();
}

#[tokio::test]
async fn a_read_error_keeps_the_active_config() {
    let (mut src, server) = source(vec![
        config(1, json!({"mode": "full"})),
        (
            500,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": 500}),
        ),
    ]);
    src.refresh().await.unwrap();
    assert!(src.refresh().await.is_err());
    assert_eq!(src.state().active().mode, Mode::Full);
    server.await.unwrap();
}

#[tokio::test]
async fn status_is_merge_patched_with_valid_false() {
    let (mut src, server) = source(vec![
        config(1, json!({})),
        config(2, json!({"cycleInterval": "5s"})),
        config(2, json!({})),
    ]);
    src.refresh().await.unwrap();
    src.refresh().await.unwrap();
    let counts = Counts {
        reaped: 1,
        ..Counts::default()
    };
    src.write_status(counts, Timestamp(1_000), true)
        .await
        .unwrap();

    let seen = server.await.unwrap();
    let patch = &seen[2];
    assert_eq!(patch.method, Method::PATCH);
    assert_eq!(patch.path, format!("{PATH}/status"));
    assert_eq!(patch.content_type, "application/merge-patch+json");
    let status = &patch.body["status"];
    assert_eq!(status["observedGeneration"], 2);
    assert_eq!(status["activeGeneration"], 1);
    assert_eq!(status["lastCycleTime"], "1970-01-01T00:16:40Z");
    assert_eq!(status["counts"]["reaped"], 1);
    let valid = &status["conditions"][0];
    assert_eq!(valid["type"], "Valid");
    assert_eq!(valid["status"], "False");
    assert_eq!(valid["observedGeneration"], 2);
    assert_eq!(status["conditions"][1]["type"], "Ready");
}

#[tokio::test]
async fn no_object_means_no_status_write() {
    let (mut src, server) = source(vec![not_found()]);
    assert_eq!(src.refresh().await.unwrap(), None);
    src.write_status(Counts::default(), Timestamp(1_000), true)
        .await
        .unwrap();
    assert_eq!(server.await.unwrap().len(), 1);
}
