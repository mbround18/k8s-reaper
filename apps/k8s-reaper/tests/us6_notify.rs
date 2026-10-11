//! US6 on the k3d dev cluster (SC-009). Opt-in:
//!
//! ```sh
//! make dev-cluster dev-push dev-run dev-notify
//! make test-integration STORY=us6_notify
//! ```
//!
//! Needs the reaper running in report mode with the Secret from
//! `deploy/dev/notify/` mounted, and `notifications.reasons: [ReapPlanned]`.
//! Reads only. Not covered here: deleting the Secret stops messages without a
//! restart (check by hand: `reaper_notification_channels` empties).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{ListParams, LogParams};
use kube::{Api, Client, Resource};
use reaper_core::record::STATE_LABEL;
use serde_json::Value;

const DEADLINE: Duration = Duration::from_mins(12);
const POLL: Duration = Duration::from_secs(5);
/// SC-009: a notified transition reaches every channel within a minute.
const DELIVERY: Duration = Duration::from_mins(1);
const SUBJECT: &str = "[k8s-reaper] ReapPlanned Deployment/plain/crashloop";

/// Mailpit's message list, through the API server's service proxy.
async fn mails(client: &Client) -> Vec<Value> {
    let request = http::Request::get(
        "/api/v1/namespaces/notify-dev/services/http:mailpit:http/proxy/api/v1/messages?limit=500",
    )
    .body(Vec::new())
    .unwrap();
    let body = client.request_text(request).await.expect("Mailpit API");
    let list: Value = serde_json::from_str(&body).expect("Mailpit returns JSON");
    list["messages"].as_array().cloned().unwrap_or_default()
}

fn subjects(mails: &[Value]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for mail in mails {
        if let Some(s) = mail["Subject"].as_str() {
            *out.entry(s.to_owned()).or_default() += 1;
        }
    }
    out
}

/// Everything the echo server logged; request bodies appear JSON-escaped.
async fn echo_log(client: &Client) -> String {
    let pods: Api<Pod> = Api::namespaced(client.clone(), "notify-dev");
    let list = pods
        .list(&ListParams::default().labels("app=echo"))
        .await
        .unwrap();
    let mut out = String::new();
    for pod in list {
        let name = pod.meta().name.clone().unwrap();
        out.push_str(&pods.logs(&name, &LogParams::default()).await.unwrap());
    }
    out.replace('\\', "")
}

/// Requests (one log line each) carrying `ReapPlanned` for `plain/crashloop`.
fn webhook_hits(log: &str) -> usize {
    log.lines()
        .filter(|l| l.contains(r#""reason":"ReapPlanned""#))
        .filter(|l| l.contains(r#""name":"crashloop","namespace":"plain""#))
        .count()
}

async fn is_candidate(client: &Client) -> bool {
    let d: Deployment = Api::namespaced(client.clone(), "plain")
        .get("crashloop")
        .await
        .unwrap();
    d.meta()
        .labels
        .as_ref()
        .and_then(|l| l.get(STATE_LABEL))
        .is_some_and(|s| s == "candidate")
}

#[tokio::test]
#[ignore = "needs the k3d dev cluster with the reaper and deploy/dev/notify (make dev-run dev-notify)"]
async fn report_mode_candidates_are_notified_once() {
    let client = Client::try_default()
        .await
        .expect("kubeconfig for the dev cluster");

    // Scenario 3 and SC-009: the mail arrives within a minute of the
    // transition (measured from when this test first sees the candidate).
    let started = Instant::now();
    let mut candidate_seen: Option<Instant> = None;
    let mail = loop {
        if candidate_seen.is_none() && is_candidate(&client).await {
            candidate_seen = Some(Instant::now());
        }
        let all = mails(&client).await;
        if let Some(m) = all.iter().find(|m| m["Subject"] == SUBJECT) {
            break m.clone();
        }
        if let Some(seen) = candidate_seen {
            assert!(
                seen.elapsed() < DELIVERY + POLL,
                "no mail {SUBJECT:?} within {DELIVERY:?} of the candidate"
            );
        }
        assert!(
            started.elapsed() < DEADLINE,
            "plain/crashloop never notified"
        );
        tokio::time::sleep(POLL).await;
    };
    let to: Vec<&str> = mail["To"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["Address"].as_str())
        .collect();
    assert_eq!(
        to,
        ["oncall@example.test", "sre@example.test"],
        "one mail to every to address"
    );

    // Scenario 4: the generic webhook got the documented payload.
    let log = echo_log(&client).await;
    assert!(
        log.contains(r#""schema":"reaper.boop.ninja/notification/v1""#),
        "echo server saw no generic payload"
    );
    assert!(log.contains(r#""name":"crashloop","namespace":"plain""#));

    // Independent test: one message per candidate, and nothing repeats on
    // later cycles (dev cycleInterval is 15s).
    let mail_before = subjects(&mails(&client).await);
    let hooks_before = webhook_hits(&echo_log(&client).await);
    for (subject, n) in &mail_before {
        if subject.starts_with("[k8s-reaper] ReapPlanned ") {
            assert_eq!(*n, 1, "{subject} sent {n} times");
        }
    }
    assert_eq!(
        hooks_before, 1,
        "plain/crashloop webhook sent {hooks_before} times"
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    let mail_after = subjects(&mails(&client).await);
    for (subject, n) in &mail_before {
        assert_eq!(mail_after.get(subject), Some(n), "{subject} repeated");
    }
    assert_eq!(
        webhook_hits(&echo_log(&client).await),
        1,
        "webhook repeated"
    );
}
