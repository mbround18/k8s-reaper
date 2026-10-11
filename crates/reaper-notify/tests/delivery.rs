//! T092: payload shapes and delivery rules against a local mock server
//! (FR-038, FR-040, contracts/notifications.md).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use reaper_core::Timestamp;
use reaper_core::config::{Mode, Notifications};
use reaper_core::model::{ObjectKind, ObjectRef};
use reaper_core::notify::{Notification, Notifier};
use reaper_core::ports::EventType;
use reaper_core::record::State as RecordState;
use reaper_notify::{Delivery, NotifyHandle, Provider, Settings, parse_channel};
use serde_json::{Value, json};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    query: Option<String>,
    content_type: String,
    body: Value,
}

#[derive(Clone, Copy)]
enum Reply {
    Status(u16),
    RetryAfter(u64),
    Delay(Duration),
}

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<Seen>>,
    script: Mutex<VecDeque<Reply>>,
}

impl Mock {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn answer(State(mock): State<Arc<Mock>>, req: Request) -> (StatusCode, HeaderMap) {
    let reply = mock.script.lock().unwrap().pop_front();
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    mock.seen.lock().unwrap().push(Seen {
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().map(str::to_owned),
        content_type: parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    });
    let mut headers = HeaderMap::new();
    match reply {
        None => (StatusCode::NO_CONTENT, headers),
        Some(Reply::Status(code)) => (StatusCode::from_u16(code).unwrap(), headers),
        Some(Reply::RetryAfter(secs)) => {
            headers.insert(header::RETRY_AFTER, secs.into());
            (StatusCode::TOO_MANY_REQUESTS, headers)
        }
        Some(Reply::Delay(d)) => {
            tokio::time::sleep(d).await;
            (StatusCode::NO_CONTENT, headers)
        }
    }
}

/// A mock server answering from `script`, then 204. Returns its base URL.
async fn mock(script: &[Reply]) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock::default());
    mock.script.lock().unwrap().extend(script.iter().copied());
    let app = Router::new().fallback(answer).with_state(Arc::clone(&mock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

fn fast() -> Settings {
    Settings {
        timeout: Duration::from_millis(300),
        backoff: vec![Duration::from_millis(10), Duration::from_millis(20)],
        max_retry_after: Duration::from_millis(50),
        ..Settings::default()
    }
}

type Results = mpsc::UnboundedReceiver<(String, Provider, Delivery)>;

fn start(settings: Settings, channels: &[(&str, String)]) -> (NotifyHandle, Results) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = NotifyHandle::start(
        settings,
        Arc::new(move |channel: &str, provider, delivery| {
            let _ = tx.send((channel.to_owned(), provider, delivery));
        }),
    )
    .unwrap();
    handle.set_channels(
        channels
            .iter()
            .map(|(name, url)| parse_channel(name, url).unwrap())
            .collect(),
    );
    (handle, rx)
}

async fn results(rx: &mut Results, n: usize) -> Vec<Delivery> {
    let mut out = Vec::new();
    while out.len() < n {
        let next = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for a delivery result")
            .unwrap();
        out.push(next.2);
    }
    out
}

fn notification(reason: &'static str, note: &str) -> Notification {
    Notification {
        reason,
        event_type: EventType::Warning,
        regarding: ObjectRef {
            kind: ObjectKind::Deployment,
            namespace: Some("plain".into()),
            name: "crashloop".into(),
        },
        state: Some(RecordState::Candidate),
        mode: Mode::Report,
        note: note.into(),
        time: Timestamp::parse_rfc3339("2026-10-10T16:00:00Z").unwrap(),
    }
}

fn config() -> Notifications {
    Notifications {
        cluster_name: "roanoke".into(),
        ..Notifications::default()
    }
}

#[tokio::test]
async fn generic_payload_matches_the_contract() {
    let (base, mock) = mock(&[]).await;
    let (notifier, mut rx) = start(
        fast(),
        &[(
            "hook",
            format!("{base}/hooks/reaper?insecure=true&token=abc"),
        )],
    );
    notifier.enqueue(notification("ReapPlanned", "restarted 14 times"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);

    let seen = mock.seen();
    assert_eq!(seen[0].path, "/hooks/reaper");
    assert_eq!(
        seen[0].query.as_deref(),
        Some("token=abc"),
        "insecure is stripped"
    );
    assert_eq!(seen[0].content_type, "application/json");
    assert_eq!(
        seen[0].body,
        json!({
            "schema": "reaper.boop.ninja/notification/v1",
            "reason": "ReapPlanned",
            "type": "Warning",
            "workload": { "kind": "Deployment", "namespace": "plain", "name": "crashloop" },
            "state": "candidate",
            "mode": "report",
            "note": "restarted 14 times",
            "time": "2026-10-10T16:00:00Z",
            "cluster": "roanoke"
        })
    );
}

#[tokio::test]
async fn discord_payload_has_content_and_one_embed() {
    let (base, mock) = mock(&[]).await;
    let (notifier, mut rx) = start(
        fast(),
        &[(
            "d",
            format!("{base}/api/webhooks/1/x?insecure=true&format=discord"),
        )],
    );
    let long = "x".repeat(3000);
    notifier.enqueue(notification("Reaped", &long), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);

    let body = &mock.seen()[0].body;
    assert_eq!(
        mock.seen()[0].query,
        None,
        "format and insecure are stripped"
    );
    let content = body["content"].as_str().unwrap();
    assert!(content.starts_with("[roanoke] Reaped Deployment plain/crashloop: x"));
    assert_eq!(content.chars().count(), 2000);
    assert_eq!(body["embeds"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["embeds"][0]["title"],
        "Reaped Deployment plain/crashloop"
    );
    assert_eq!(body["embeds"][0]["timestamp"], "2026-10-10T16:00:00Z");
    assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
}

#[tokio::test]
async fn slack_payload_is_text_only() {
    let (base, mock) = mock(&[]).await;
    let (notifier, mut rx) = start(
        fast(),
        &[("s", format!("{base}/services/A?insecure=true&format=slack"))],
    );
    notifier.enqueue(notification("Reaped", "scaled to 0"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);
    assert_eq!(
        mock.seen()[0].body,
        json!({ "text": "[roanoke] Reaped Deployment plain/crashloop: scaled to 0" })
    );
}

#[tokio::test]
async fn server_errors_are_retried_then_succeed() {
    let (base, mock) = mock(&[Reply::Status(500), Reply::Status(503)]).await;
    let (notifier, mut rx) = start(fast(), &[("hook", format!("{base}/h?insecure=true"))]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);
    assert_eq!(mock.seen().len(), 3);
}

#[tokio::test]
async fn attempts_are_bounded() {
    let (base, mock) = mock(&[Reply::Status(500); 5]).await;
    let (notifier, mut rx) = start(fast(), &[("hook", format!("{base}/h?insecure=true"))]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Failed]);
    assert_eq!(mock.seen().len(), 3);
}

#[tokio::test]
async fn client_errors_are_not_retried_except_408() {
    let (base, mock) = mock(&[Reply::Status(404)]).await;
    let (notifier, mut rx) = start(fast(), &[("hook", format!("{base}/h?insecure=true"))]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Failed]);
    assert_eq!(mock.seen().len(), 1);

    let (base, mock) = self::mock(&[Reply::Status(408)]).await;
    notifier.set_channels(vec![
        parse_channel("hook", &format!("{base}/h?insecure=true")).unwrap(),
    ]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);
    assert_eq!(mock.seen().len(), 2);
}

#[tokio::test]
async fn retry_after_is_honoured_up_to_the_cap() {
    // A backoff far longer than the test's patience: only Retry-After, capped
    // at 50 ms, can bring the second attempt in time.
    let settings = Settings {
        backoff: vec![Duration::from_secs(60), Duration::from_secs(60)],
        ..fast()
    };
    let (base, mock) = mock(&[Reply::RetryAfter(3600)]).await;
    let (notifier, mut rx) = start(settings, &[("hook", format!("{base}/h?insecure=true"))]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);
    assert_eq!(mock.seen().len(), 2);
}

#[tokio::test]
async fn a_slow_attempt_times_out_and_is_retried() {
    let (base, mock) = mock(&[Reply::Delay(Duration::from_secs(3))]).await;
    let (notifier, mut rx) = start(fast(), &[("hook", format!("{base}/h?insecure=true"))]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);
    assert_eq!(mock.seen().len(), 2);
}

#[tokio::test]
async fn overflow_drops_the_oldest() {
    let (base, mock) = mock(&[]).await;
    let settings = Settings {
        queue_capacity: 2,
        ..fast()
    };
    let (notifier, mut rx) = start(settings, &[("hook", format!("{base}/h?insecure=true"))]);
    // The test runtime is single-threaded: the worker cannot take anything
    // until this test awaits, so all four land in the queue first.
    for note in ["one", "two", "three", "four"] {
        notifier.enqueue(notification("Reaped", note), &config());
    }
    let mut got = results(&mut rx, 4).await;
    got.sort_by_key(|d| d.as_str());
    assert_eq!(
        got,
        [
            Delivery::Dropped,
            Delivery::Dropped,
            Delivery::Sent,
            Delivery::Sent
        ]
    );
    let mut notes: Vec<String> = mock
        .seen()
        .iter()
        .map(|s| s.body["note"].as_str().unwrap().to_owned())
        .collect();
    notes.sort();
    assert_eq!(notes, ["four", "three"]);
}

#[tokio::test]
async fn per_channel_reasons_filter() {
    let (base, mock) = mock(&[]).await;
    let (notifier, mut rx) = start(
        fast(),
        &[
            ("all", format!("{base}/all?insecure=true")),
            ("failures", format!("{base}/failures?insecure=true")),
        ],
    );
    let mut config = config();
    config
        .channels
        .insert("failures".into(), ["ReapFailed".to_owned()].into());
    notifier.enqueue(notification("Reaped", "n"), &config);
    let mut got = results(&mut rx, 2).await;
    got.sort_by_key(|d| d.as_str());
    assert_eq!(got, [Delivery::Filtered, Delivery::Sent]);
    let paths: Vec<String> = mock.seen().iter().map(|s| s.path.clone()).collect();
    assert_eq!(paths, ["/all"]);
}

#[tokio::test]
async fn shutdown_drains_the_queue() {
    let (base, mock) = mock(&[]).await;
    let (notifier, mut rx) = start(fast(), &[("hook", format!("{base}/h?insecure=true"))]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(notifier.shutdown(Duration::from_secs(5)).await, 0);
    assert_eq!(results(&mut rx, 1).await, [Delivery::Sent]);
    assert_eq!(mock.seen().len(), 1);

    notifier.enqueue(notification("Reaped", "after"), &config());
    assert_eq!(results(&mut rx, 1).await, [Delivery::Dropped]);
}

#[tokio::test]
async fn no_channels_means_no_requests() {
    let (_base, mock) = mock(&[]).await;
    let (notifier, mut rx) = start(fast(), &[]);
    notifier.enqueue(notification("Reaped", "n"), &config());
    assert_eq!(notifier.shutdown(Duration::from_secs(1)).await, 0);
    assert!(rx.try_recv().is_err());
    assert_eq!(mock.seen().len(), 0);
}
